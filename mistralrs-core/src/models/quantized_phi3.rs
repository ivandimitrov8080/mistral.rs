#![allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]

use crate::device_map::DeviceMapper;
use crate::layers::CausalMasker;
use crate::layers::RmsNorm;
use crate::DeviceMapMetadata;
use candle_core::quantized::gguf_file;
use candle_core::quantized::QMatMul;
use candle_core::quantized::QTensor;
use candle_core::{DType, Device, IndexOp, Module, Result, Tensor, D};
use candle_nn::Embedding;

use super::repeat_kv;
use super::verify_sanity_gguf;
use super::Cache;

#[derive(Debug, Clone)]
struct Mlp {
    ffn_up: QMatMul,
    ffn_down: QMatMul,
    i_size: usize,
}

impl Module for Mlp {
    fn forward(&self, xs: &Tensor) -> Result<Tensor> {
        let up_states = xs.apply(&self.ffn_up)?;
        let gate = up_states.narrow(D::Minus1, 0, self.i_size)?;
        let up_states = up_states.narrow(D::Minus1, self.i_size, self.i_size)?;
        let up_states = (up_states * gate.silu()?)?;
        up_states.apply(&self.ffn_down)
    }
}

fn rms_norm(w: QTensor, eps: f64) -> Result<RmsNorm> {
    let w = w.dequantize(&w.device())?;
    let rms = RmsNorm::from_w(w, eps)?;
    Ok(rms)
}

#[derive(Debug, Clone)]
struct LayerWeights {
    attn_qkv: QMatMul,
    attn_output: QMatMul,
    attn_norm: RmsNorm,
    ffn_norm: RmsNorm,
    mlp: Mlp,
    n_head: usize,
    n_kv_head: usize,
    head_dim: usize,
    cos: Tensor,
    sin: Tensor,
    neg_inf: Tensor,
    sliding_window: usize,
}

impl LayerWeights {
    fn apply_rotary_emb(&self, xs: &Tensor, seqlen_offsets: &[usize]) -> Result<Tensor> {
        let (_b_sz, _h, seq_len, _n_embd) = xs.dims4()?;
        let mut outputs = Vec::new();
        for (i, offset) in seqlen_offsets.iter().enumerate() {
            let cos = self.cos.narrow(0, *offset, seq_len)?;
            let sin = self.sin.narrow(0, *offset, seq_len)?;
            outputs.push(candle_nn::rotary_emb::rope(
                &xs.i(i)?.unsqueeze(0)?.contiguous()?,
                &cos,
                &sin,
            )?);
        }
        Tensor::cat(&outputs, 0)
    }

    fn forward_attn(
        &mut self,
        x: &Tensor,
        mask: Option<&Tensor>,
        seqlen_offsets: &[usize],
        kv_cache: &mut Option<(Tensor, Tensor)>,
    ) -> Result<Tensor> {
        let (b_sz, seq_len, n_embd) = x.dims3()?;
        let qkv = self.attn_qkv.forward(x)?;

        let query_pos = self.n_head * self.head_dim;
        let q = qkv.narrow(D::Minus1, 0, query_pos)?;
        let k = qkv.narrow(D::Minus1, query_pos, self.n_kv_head * self.head_dim)?;
        let v = qkv.narrow(
            D::Minus1,
            query_pos + self.n_kv_head * self.head_dim,
            self.n_kv_head * self.head_dim,
        )?;

        let q = q
            .reshape((b_sz, seq_len, self.n_head, self.head_dim))?
            .transpose(1, 2)?;
        let k = k
            .reshape((b_sz, seq_len, self.n_head, self.head_dim))?
            .transpose(1, 2)?;
        let v = v
            .reshape((b_sz, seq_len, self.n_kv_head, self.head_dim))?
            .transpose(1, 2)?;

        let q = self.apply_rotary_emb(&q, seqlen_offsets)?.contiguous()?;
        let k = self.apply_rotary_emb(&k, seqlen_offsets)?;

        let (k, v, attn_mask) = match kv_cache.clone() {
            None => (k, v, mask.cloned()),
            Some((mut prev_k, mut prev_v)) => {
                let mut mask = mask.cloned();
                let kv_seq_len = prev_k.dim(2)?;
                let sliding_window = self.sliding_window;
                if kv_seq_len > sliding_window {
                    prev_k =
                        prev_k.narrow(2, kv_seq_len - (sliding_window - 1), sliding_window - 1)?;
                    prev_v =
                        prev_v.narrow(2, kv_seq_len - (sliding_window - 1), sliding_window - 1)?;
                    if let Some(ref mut mask) = mask {
                        let mask_len = mask.dim(1)?;
                        *mask =
                            mask.narrow(1, mask_len - (sliding_window - 1), sliding_window - 1)?;
                        *mask = Tensor::cat(
                            &[&*mask, &mask.narrow(1, mask_len - 1, 1)?.ones_like()?],
                            D::Minus1,
                        )?;
                    }
                }
                let k = Tensor::cat(&[prev_k, k], 2)?;
                let v = Tensor::cat(&[prev_v, v], 2)?;
                (k, v, mask)
            }
        };
        *kv_cache = Some((k.clone(), v.clone()));

        let k = repeat_kv(k, self.n_head / self.n_kv_head)?;
        let v = repeat_kv(v, self.n_head / self.n_kv_head)?;

        let att = (q.matmul(&k.t()?)? / (self.head_dim as f64).sqrt())?;
        let att = CausalMasker.apply_mask(&attn_mask, att, &self.neg_inf)?;
        let att = candle_nn::ops::softmax_last_dim(&att)?;
        // Convert to contiguous as matmul doesn't support strided vs for now.
        let y = att.matmul(&v.contiguous()?)?;
        let y = y.transpose(1, 2)?.reshape(&[b_sz, seq_len, n_embd])?;
        let y = self.attn_output.forward(&y)?;
        Ok(y)
    }
}

#[derive(Debug)]
pub struct ModelWeights {
    tok_embeddings: Embedding,
    layers: Vec<LayerWeights>,
    output_norm: RmsNorm,
    output: QMatMul,
    mapper: Option<Box<dyn DeviceMapper + Send + Sync>>,
    pub device: Device,
    pub cache: Cache,
    pub max_seq_len: usize,
}

fn precomput_freqs_cis(
    head_dim: usize,
    freq_base: f32,
    device: &Device,
    context_window: usize,
) -> Result<(Tensor, Tensor)> {
    let theta: Vec<_> = (0..head_dim)
        .step_by(2)
        .map(|i| 1f32 / freq_base.powf(i as f32 / head_dim as f32))
        .collect();
    let theta = Tensor::new(theta.as_slice(), device)?;
    let idx_theta = Tensor::arange(0, context_window as u32, device)?
        .to_dtype(DType::F32)?
        .reshape((context_window, 1))?
        .matmul(&theta.reshape((1, theta.elem_count()))?)?;
    let cos = idx_theta.cos()?;
    let sin = idx_theta.sin()?;
    Ok((cos, sin))
}

impl ModelWeights {
    pub fn from_gguf<R: std::io::Seek + std::io::Read>(
        ct: gguf_file::Content,
        reader: &mut R,
        device: &Device,
        mapper: DeviceMapMetadata,
    ) -> Result<Self> {
        let md_get = |s: &str| match ct.metadata.get(s) {
            None => candle_core::bail!("cannot find {s} in metadata"),
            Some(v) => Ok(v),
        };
        verify_sanity_gguf(md_get("general.architecture")?.to_string().unwrap(), "phi3")?;

        // Parameter extraction from metadata.
        let head_count = md_get("phi3.attention.head_count")?.to_u32()? as usize;
        let head_count_kv = md_get("phi3.attention.head_count_kv")?.to_u32()? as usize;
        let block_count = md_get("phi3.block_count")?.to_u32()? as usize;
        let embedding_length = md_get("phi3.embedding_length")?.to_u32()? as usize;
        let i_size = md_get("phi3.feed_forward_length")?.to_u32()? as usize;
        let rope_dim = md_get("phi3.rope.dimension_count")?.to_u32()? as usize;
        let rms_eps = md_get("phi3.attention.layer_norm_rms_epsilon")?.to_f32()? as f64;
        let context_window = md_get("phi3.context_length")?.to_u32()? as usize;
        let (cos, sin) = precomput_freqs_cis(rope_dim, 10_000., device, context_window)?;
        let neg_inf = Tensor::new(f32::NEG_INFINITY, device)?;

        let tok_embeddings = ct.tensor(reader, "token_embd.weight", device)?;
        let tok_embeddings = tok_embeddings.dequantize(device)?;
        let output_norm = rms_norm(ct.tensor(reader, "output_norm.weight", device)?, rms_eps)?;
        let output = QMatMul::from_qtensor(ct.tensor(reader, "output.weight", device)?)?;
        let mut layers = Vec::with_capacity(block_count);
        let mapper = mapper.into_mapper(block_count, device)?;
        for layer_idx in 0..block_count {
            let prefix = format!("blk.{layer_idx}");
            let device = mapper.device_for(layer_idx, false).unwrap_or(device);
            let ffn_up = QMatMul::from_qtensor(ct.tensor(
                reader,
                &format!("{prefix}.ffn_up.weight"),
                device,
            )?)?;
            let ffn_down = QMatMul::from_qtensor(ct.tensor(
                reader,
                &format!("{prefix}.ffn_down.weight"),
                device,
            )?)?;
            let mlp = Mlp {
                ffn_up,
                ffn_down,
                i_size,
            };
            let attn_norm = rms_norm(
                ct.tensor(reader, &format!("{prefix}.attn_norm.weight"), device)?,
                rms_eps,
            )?;
            let ffn_norm = rms_norm(
                ct.tensor(reader, &format!("{prefix}.ffn_norm.weight"), device)?,
                rms_eps,
            )?;
            layers.push(LayerWeights {
                attn_qkv: QMatMul::from_qtensor(ct.tensor(
                    reader,
                    &format!("{prefix}.attn_qkv.weight"),
                    device,
                )?)?,
                attn_output: QMatMul::from_qtensor(ct.tensor(
                    reader,
                    &format!("{prefix}.attn_output.weight"),
                    device,
                )?)?,
                attn_norm,
                ffn_norm,
                mlp,
                n_head: head_count,
                n_kv_head: head_count_kv,
                head_dim: embedding_length / head_count,
                cos: cos.clone(),
                sin: sin.clone(),
                neg_inf: neg_inf.clone(),
                sliding_window: context_window,
            })
        }
        Ok(Self {
            tok_embeddings: Embedding::new(tok_embeddings, embedding_length),
            layers,
            output_norm,
            output,
            mapper: Some(mapper),
            device: device.clone(),
            cache: Cache::new(block_count, false),
            max_seq_len: context_window,
        })
    }

    pub fn forward(&mut self, xs: &Tensor, seqlen_offsets: &[usize]) -> Result<Tensor> {
        let (_b_sz, seq_len) = xs.dims2()?;
        let mask = CausalMasker.make_causal_mask_with_sliding_window(
            xs,
            &self.cache,
            Some(self.max_seq_len),
        )?;
        let mut xs = self.tok_embeddings.forward(xs)?;
        let mut cache = self.cache.lock();
        for (i, layer) in self.layers.iter_mut().enumerate() {
            if let Some(ref mapper) = self.mapper {
                xs = mapper.map(xs, i)?;
            }
            let residual = &xs;
            let ys = xs.apply(&layer.attn_norm)?;
            let ys = layer.forward_attn(
                &ys,
                mask.as_ref()
                    .map(|m| m.to_device(xs.device()).unwrap())
                    .as_ref(),
                seqlen_offsets,
                &mut cache[i],
            )?;
            let ys = (ys + residual)?;
            let residual = &ys;
            let ys = ys.apply(&layer.ffn_norm)?;
            let ys = layer.mlp.forward(&ys)?;
            xs = (ys + residual)?
        }
        let xs = xs.to_device(&self.device)?;
        let xs = xs.apply(&self.output_norm)?.i((.., seq_len - 1, ..))?;
        self.output.forward(&xs)
    }
}
