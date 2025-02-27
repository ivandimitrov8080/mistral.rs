#![allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]

use candle_core::{quantized::QMatMul, DType, Device, Result, Tensor, D};
use candle_nn::{
    embedding, linear_no_bias as linear, Embedding, Module, RotaryEmbedding, VarBuilder,
};
use serde::Deserialize;
use std::sync::Arc;

use crate::{
    device_map::DeviceMapper,
    layers::{CausalMasker, RmsNorm},
    pipeline::{extract_logits, NormalModel},
    DeviceMapMetadata,
};

use super::{flash_attn, repeat_kv};

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub hidden_size: usize,
    pub intermediate_size: usize,
    pub vocab_size: usize,
    pub num_hidden_layers: usize,
    pub num_attention_heads: usize,
    pub num_key_value_heads: usize,
    pub use_flash_attn: bool,
    pub rms_norm_eps: f64,
    pub rope_theta: f32,
    pub max_position_embeddings: usize,
}

#[derive(Debug, Clone)]
struct CausalSelfAttention {
    q_proj: QMatMul,
    k_proj: QMatMul,
    v_proj: QMatMul,
    o_proj: QMatMul,
    num_attention_heads: usize,
    num_key_value_heads: usize,
    head_dim: usize,
    use_flash_attn: bool,
    rotary_emb: Arc<RotaryEmbedding>,
    max_seq_len: usize,
    neg_inf: Tensor,
}

impl CausalSelfAttention {
    fn forward(
        &self,
        x: &Tensor,
        attention_mask: &Option<Tensor>,
        seqlen_offsets: &[usize],
        start_offsets_kernel: Tensor,
        block_idx: usize,
        kv_cache: &mut super::LayerCaches,
    ) -> Result<Tensor> {
        let (b_sz, seq_len, hidden_size) = x.dims3()?;

        let original_dtype = x.dtype();
        let mut x = x.clone();
        if matches!(self.q_proj, QMatMul::QTensor(_)) {
            x = x.to_dtype(DType::F32)?;
        }
        let mut q = self.q_proj.forward(&x)?;
        let mut k = self.k_proj.forward(&x)?;
        let mut v = self.v_proj.forward(&x)?;
        if matches!(self.q_proj, QMatMul::QTensor(_)) {
            q = q.to_dtype(original_dtype)?;
            k = k.to_dtype(original_dtype)?;
            v = v.to_dtype(original_dtype)?;
        }

        let mut q = q.reshape((b_sz * seq_len, self.num_attention_heads, self.head_dim))?;
        let mut k = k.reshape((b_sz * seq_len, self.num_key_value_heads, self.head_dim))?;
        let mut v = v
            .reshape((b_sz, seq_len, self.num_key_value_heads, self.head_dim))?
            .transpose(1, 2)?;

        self.rotary_emb
            .forward(seqlen_offsets, &start_offsets_kernel, &mut q, &mut k, b_sz)?;

        if q.rank() == 3 {
            q = q
                .reshape((b_sz, seq_len, self.num_attention_heads, self.head_dim))?
                .transpose(1, 2)?
                .contiguous()?;
            k = k
                .reshape((b_sz, seq_len, self.num_key_value_heads, self.head_dim))?
                .transpose(1, 2)?
                .contiguous()?;
        }

        if let Some((cache_k, cache_v)) = &kv_cache[block_idx] {
            k = candle_nn::ops::kvconcat(cache_k, &k, 2)?.contiguous()?;
            v = candle_nn::ops::kvconcat(cache_v, &v, 2)?.contiguous()?;
            let k_seq_len = k.dims()[1];
            if k_seq_len > self.max_seq_len {
                k = k
                    .narrow(D::Minus1, k_seq_len - self.max_seq_len, self.max_seq_len)?
                    .contiguous()?
            }
            let v_seq_len = v.dims()[1];
            if v_seq_len > 2 * self.max_seq_len {
                v = v
                    .narrow(D::Minus1, v_seq_len - self.max_seq_len, self.max_seq_len)?
                    .contiguous()?
            }
        }
        kv_cache[block_idx] = Some((k.clone(), v.clone()));

        let k = repeat_kv(k, self.num_attention_heads / self.num_key_value_heads)?.contiguous()?;
        let v = repeat_kv(v, self.num_attention_heads / self.num_key_value_heads)?.contiguous()?;

        let mut y = if self.use_flash_attn {
            // flash-attn expects (b_sz, seq_len, nheads, head_dim)
            let q = q.transpose(1, 2)?;
            let k = k.transpose(1, 2)?;
            let v = v.transpose(1, 2)?;
            let softmax_scale = 1f32 / (self.head_dim as f32).sqrt();
            flash_attn(&q, &k, &v, softmax_scale, seq_len > 1)?.transpose(1, 2)?
        } else {
            let in_dtype = q.dtype();
            let q = q.to_dtype(DType::F32)?;
            let k = k.to_dtype(DType::F32)?;
            let v = v.to_dtype(DType::F32)?;
            let att = (q.matmul(&k.t()?)? / (self.head_dim as f64).sqrt())?;
            let att = CausalMasker.apply_mask(attention_mask, att, &self.neg_inf)?;
            let att = candle_nn::ops::softmax(&att, D::Minus1)?;
            // Convert to contiguous as matmul doesn't support strided vs for now.
            att.matmul(&v.contiguous()?)?.to_dtype(in_dtype)?
        };
        if matches!(self.q_proj, QMatMul::QTensor(_)) {
            y = y.to_dtype(DType::F32)?;
        }
        let y = y.transpose(1, 2)?.reshape(&[b_sz, seq_len, hidden_size])?;
        let mut y = self.o_proj.forward(&y)?;
        if matches!(self.q_proj, QMatMul::QTensor(_)) {
            y = y.to_dtype(original_dtype)?;
        }
        Ok(y)
    }

    fn load(vb: VarBuilder, cfg: &Config, rope: Arc<RotaryEmbedding>) -> Result<Self> {
        let size_in = cfg.hidden_size;
        let size_q = (cfg.hidden_size / cfg.num_attention_heads) * cfg.num_attention_heads;
        let size_kv = (cfg.hidden_size / cfg.num_attention_heads) * cfg.num_key_value_heads;
        let q_proj = linear(size_in, size_q, vb.pp("q_proj"))?;
        let k_proj = linear(size_in, size_kv, vb.pp("k_proj"))?;
        let v_proj = linear(size_in, size_kv, vb.pp("v_proj"))?;
        let o_proj = linear(size_q, size_in, vb.pp("o_proj"))?;
        Ok(Self {
            q_proj: QMatMul::Tensor(q_proj.weight().clone()),
            k_proj: QMatMul::Tensor(k_proj.weight().clone()),
            v_proj: QMatMul::Tensor(v_proj.weight().clone()),
            o_proj: QMatMul::Tensor(o_proj.weight().clone()),
            num_attention_heads: cfg.num_attention_heads,
            num_key_value_heads: cfg.num_key_value_heads,
            head_dim: cfg.hidden_size / cfg.num_attention_heads,
            use_flash_attn: cfg.use_flash_attn,
            rotary_emb: rope,
            max_seq_len: cfg.max_position_embeddings,
            neg_inf: Tensor::new(f32::NEG_INFINITY, vb.device())?.to_dtype(vb.dtype())?,
        })
    }
}

#[derive(Debug, Clone)]
struct Mlp {
    c_fc1: QMatMul,
    c_fc2: QMatMul,
    c_proj: QMatMul,
}

impl Mlp {
    fn forward(&self, x: &Tensor) -> Result<Tensor> {
        let original_dtype = x.dtype();
        let mut x = x.clone();
        if matches!(self.c_fc1, QMatMul::QTensor(_)) {
            x = x.to_dtype(DType::F32)?;
        }
        let x = (candle_nn::ops::silu(&self.c_fc1.forward(&x)?)? * self.c_fc2.forward(&x)?)?;
        let mut res = self.c_proj.forward(&x)?;
        if matches!(self.c_fc1, QMatMul::QTensor(_)) {
            res = res.to_dtype(original_dtype)?;
        }
        Ok(res)
    }

    fn load(vb: VarBuilder, cfg: &Config) -> Result<Self> {
        let h_size = cfg.hidden_size;
        let i_size = cfg.intermediate_size;
        let c_fc1 = linear(h_size, i_size, vb.pp("gate_proj"))?;
        let c_fc2 = linear(h_size, i_size, vb.pp("up_proj"))?;
        let c_proj = linear(i_size, h_size, vb.pp("down_proj"))?;
        Ok(Self {
            c_fc1: QMatMul::Tensor(c_fc1.weight().clone()),
            c_fc2: QMatMul::Tensor(c_fc2.weight().clone()),
            c_proj: QMatMul::Tensor(c_proj.weight().clone()),
        })
    }
}

#[derive(Debug, Clone)]
struct Block {
    rms_1: RmsNorm,
    attn: CausalSelfAttention,
    rms_2: RmsNorm,
    mlp: Mlp,
}

impl Block {
    fn forward(
        &self,
        x: &Tensor,
        attention_mask: &Option<Tensor>,
        seqlen_offsets: &[usize],
        start_offsets_kernel: Tensor,
        block_idx: usize,
        kv_cache: &mut super::LayerCaches,
    ) -> Result<Tensor> {
        let residual = x;
        let x = self.rms_1.forward(x)?;
        let x = (self.attn.forward(
            &x,
            attention_mask,
            seqlen_offsets,
            start_offsets_kernel,
            block_idx,
            kv_cache,
        )? + residual)?;
        let residual = &x;
        let x = (self.mlp.forward(&self.rms_2.forward(&x)?)? + residual)?;
        Ok(x)
    }

    fn load(
        vb: VarBuilder,
        cfg: &Config,
        mapper: &dyn DeviceMapper,
        layer_idx: usize,
        loading_isq: bool,
        rope: Arc<RotaryEmbedding>,
    ) -> Result<Self> {
        let attn = CausalSelfAttention::load(
            mapper.set_device(layer_idx, vb.pp("self_attn"), loading_isq),
            cfg,
            rope,
        )?;
        let mlp = Mlp::load(mapper.set_device(layer_idx, vb.pp("mlp"), loading_isq), cfg)?;
        let rms_1 = RmsNorm::new(
            cfg.hidden_size,
            cfg.rms_norm_eps,
            mapper.set_nm_device(vb.pp("input_layernorm"), false),
        )?;
        let rms_2 = RmsNorm::new(
            cfg.hidden_size,
            cfg.rms_norm_eps,
            mapper.set_nm_device(vb.pp("post_attention_layernorm"), false),
        )?;
        Ok(Self {
            rms_1,
            attn,
            rms_2,
            mlp,
        })
    }
}

#[derive(Debug)]
pub struct Llama {
    wte: Embedding,
    blocks: Vec<Block>,
    ln_f: RmsNorm,
    lm_head: QMatMul,
    pub kv_cache: super::Cache,
    pub device: Device,
    mapper: Box<dyn DeviceMapper + Send + Sync>,
}

impl Llama {
    pub fn forward(
        &mut self,
        x: &Tensor,
        seqlen_offsets: &[usize],
        start_offsets_kernel: Tensor,
        context_lens: Vec<(usize, usize)>,
    ) -> Result<Tensor> {
        let mask = CausalMasker.make_causal_mask(x, &self.kv_cache)?;
        let mut x = self.wte.forward(x)?;
        let mut cache = self.kv_cache.lock();
        for (block_idx, block) in self.blocks.iter().enumerate() {
            x = self.mapper.map(x, block_idx)?;
            x = block.forward(
                &x,
                &mask.clone().map(|m| m.to_device(x.device()).unwrap()),
                seqlen_offsets,
                start_offsets_kernel.clone(),
                block_idx,
                &mut cache,
            )?;
        }
        let x = x.to_device(&self.device)?;
        let mut x = self.ln_f.forward(&x)?;
        if matches!(self.lm_head, QMatMul::QTensor(_)) {
            x = x.to_dtype(DType::F32)?;
        }
        let logits = self.lm_head.forward(&x)?;
        extract_logits(&logits, context_lens)
    }

    pub fn new(
        cfg: &Config,
        vb: VarBuilder,
        is_gptx: bool,
        mapper: DeviceMapMetadata,
        loading_isq: bool,
        real_device: Device,
    ) -> Result<Self> {
        let mapper = mapper.into_mapper(cfg.num_hidden_layers, &real_device)?;
        let wte = embedding(
            cfg.vocab_size,
            cfg.hidden_size,
            mapper.set_nm_device(vb.pp("model.embed_tokens"), false),
        )?;
        let lm_head = linear(
            cfg.hidden_size,
            cfg.vocab_size,
            mapper.set_nm_device(vb.pp("lm_head"), loading_isq),
        )?;
        let ln_f = RmsNorm::new(
            cfg.hidden_size,
            cfg.rms_norm_eps,
            mapper.set_nm_device(vb.pp("model.norm"), false),
        )?;
        let head_dim = cfg.hidden_size / cfg.num_attention_heads;
        let blocks: Vec<_> = (0..cfg.num_hidden_layers)
            .map(|i| {
                let rotary_emb = Arc::new(
                    RotaryEmbedding::new(
                        cfg.rope_theta,
                        head_dim,
                        cfg.max_position_embeddings,
                        mapper.device_for(i, false).unwrap_or(&real_device),
                        is_gptx,
                        vb.dtype(),
                    )
                    .expect("Failed to create RoPE"),
                );
                Block::load(
                    vb.pp(&format!("model.layers.{i}")),
                    cfg,
                    &*mapper,
                    i,
                    loading_isq,
                    rotary_emb,
                )
                .expect("Failed to load block.")
            })
            .collect();

        Ok(Self {
            wte,
            blocks,
            ln_f,
            lm_head: QMatMul::Tensor(lm_head.weight().clone()),
            kv_cache: super::Cache::new(cfg.num_hidden_layers, false),
            device: real_device,
            mapper,
        })
    }
}

impl NormalModel for Llama {
    fn forward(
        &mut self,
        input_ids: &Tensor,
        seqlen_offsets: &[usize],
        start_offsets_kernel: Tensor,
        context_lens: Vec<(usize, usize)>,
        _position_ids: Vec<usize>,
    ) -> Result<Tensor> {
        self.forward(
            input_ids,
            seqlen_offsets,
            start_offsets_kernel,
            context_lens,
        )
    }
    fn xlora_forward(
        &mut self,
        _input_ids: &Tensor,
        _input_ids_full: &Tensor,
        _seqlen_offsets: &[usize],
        _seqlen_offsets_full: &[usize],
        _start_offsets_kernel: Tensor,
        _start_offsets_kernel_full: Tensor,
        _no_kv_cache: bool,
        _non_granular_state: &Option<crate::xlora_models::NonGranularState>,
        _context_lens: Vec<(usize, usize)>,
        _position_ids: Vec<usize>,
    ) -> Result<Tensor> {
        unimplemented!()
    }
    fn cache(&self) -> &super::Cache {
        &self.kv_cache
    }
    fn device(&self) -> &Device {
        &self.device
    }
    fn is_xlora(&self) -> bool {
        false
    }
    fn max_seq_len(&self) -> usize {
        self.blocks[0].attn.max_seq_len
    }
    fn get_tensors(&mut self) -> (Vec<(&mut QMatMul, Option<usize>)>, &dyn DeviceMapper) {
        let mut tensors = Vec::new();
        tensors.push((&mut self.lm_head, None));
        for (i, layer) in self.blocks.iter_mut().enumerate() {
            tensors.push((&mut layer.attn.q_proj, Some(i)));
            tensors.push((&mut layer.attn.k_proj, Some(i)));
            tensors.push((&mut layer.attn.v_proj, Some(i)));
            tensors.push((&mut layer.attn.o_proj, Some(i)));
            tensors.push((&mut layer.mlp.c_fc1, Some(i)));
            tensors.push((&mut layer.mlp.c_fc2, Some(i)));
            tensors.push((&mut layer.mlp.c_proj, Some(i)));
        }
        (tensors, &*self.mapper)
    }
}
