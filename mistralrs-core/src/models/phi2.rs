#![allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]

/// Phi model.
/// https://huggingface.co/microsoft/phi-2
/// There is an alternative implementation of the phi model in mixformers.rs.
/// This corresponds to the model update made with the following commit:
/// https://huggingface.co/microsoft/phi-2/commit/cb2f4533604d8b67de604e7df03bfe6f3ca22869
use candle_core::{quantized::QMatMul, DType, Device, Module, Result, Tensor};
use candle_nn::{
    embedding, layer_norm, linear, Activation, Embedding, LayerNorm, RotaryEmbedding, VarBuilder,
};
use mistralrs_lora::layer::QLinear;
use serde::Deserialize;

use crate::{
    device_map::DeviceMapper,
    layers::CausalMasker,
    pipeline::{extract_logits, NormalModel},
    DeviceMapMetadata,
};

use super::{flash_attn, repeat_kv, Cache};

// https://huggingface.co/microsoft/phi-2/blob/main/configuration_phi.py
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Config {
    pub(crate) vocab_size: usize,
    pub(crate) hidden_size: usize,
    pub(crate) intermediate_size: usize,
    pub(crate) num_hidden_layers: usize,
    pub(crate) num_attention_heads: usize,
    pub(crate) num_key_value_heads: Option<usize>,
    pub(crate) hidden_act: Activation,
    pub(crate) max_position_embeddings: usize,
    pub(crate) layer_norm_eps: f64,
    pub(crate) tie_word_embeddings: bool,
    pub(crate) rope_theta: f32,
    pub(crate) partial_rotary_factor: f64,
    pub(crate) qk_layernorm: bool,
    pub(crate) use_flash_attn: bool,
}

impl Config {
    pub(crate) fn num_key_value_heads(&self) -> usize {
        self.num_key_value_heads.unwrap_or(self.num_attention_heads)
    }

    pub(crate) fn head_dim(&self) -> usize {
        self.hidden_size / self.num_attention_heads
    }
}

#[derive(Debug, Clone)]
#[allow(clippy::upper_case_acronyms)]
struct MLP {
    fc1: QLinear,
    fc2: QLinear,
    act: Activation,
}

impl MLP {
    fn new(cfg: &Config, vb: VarBuilder) -> Result<Self> {
        let fc1 = linear(cfg.hidden_size, cfg.intermediate_size, vb.pp("fc1"))?;
        let fc2 = linear(cfg.intermediate_size, cfg.hidden_size, vb.pp("fc2"))?;
        Ok(Self {
            fc1: QLinear::from_linear(fc1),
            fc2: QLinear::from_linear(fc2),
            // This does not match the mixformers implementation where Gelu is used rather than
            // GeluNew.
            act: cfg.hidden_act,
        })
    }
}

impl Module for MLP {
    fn forward(&self, xs: &Tensor) -> Result<Tensor> {
        let original_dtype = xs.dtype();
        let mut xs = xs.clone();
        if self.fc1.is_quant() {
            xs = xs.to_dtype(DType::F32)?;
        }
        let mut res = xs.apply(&self.fc1)?.apply(&self.act)?.apply(&self.fc2)?;
        if self.fc1.is_quant() {
            res = res.to_dtype(original_dtype)?;
        }
        Ok(res)
    }
}

#[derive(Clone)]
struct Attention {
    q_proj: QLinear,
    k_proj: QLinear,
    v_proj: QLinear,
    dense: QLinear,
    q_layernorm: Option<LayerNorm>,
    k_layernorm: Option<LayerNorm>,
    rotary_emb: RotaryEmbedding,
    softmax_scale: f64,
    num_heads: usize,
    num_kv_heads: usize,
    head_dim: usize,
    use_flash_attn: bool,
    neg_inf: Tensor,
}

impl Attention {
    fn new(cfg: &Config, vb: VarBuilder, rope: RotaryEmbedding) -> Result<Self> {
        let num_heads = cfg.num_attention_heads;
        let num_kv_heads = cfg.num_key_value_heads();
        let head_dim = cfg.head_dim();
        let q_proj = linear(cfg.hidden_size, num_heads * head_dim, vb.pp("q_proj"))?;
        let k_proj = linear(cfg.hidden_size, num_kv_heads * head_dim, vb.pp("k_proj"))?;
        let v_proj = linear(cfg.hidden_size, num_kv_heads * head_dim, vb.pp("v_proj"))?;
        let dense = linear(num_heads * head_dim, cfg.hidden_size, vb.pp("dense"))?;
        let (q_layernorm, k_layernorm) = if cfg.qk_layernorm {
            let q_layernorm = layer_norm(head_dim, cfg.layer_norm_eps, vb.pp("q_layernorm"))?;
            let k_layernorm = layer_norm(head_dim, cfg.layer_norm_eps, vb.pp("k_layernorm"))?;
            (Some(q_layernorm), Some(k_layernorm))
        } else {
            (None, None)
        };
        let softmax_scale = 1f64 / (head_dim as f64).sqrt();
        Ok(Self {
            q_proj: QLinear::from_linear(q_proj),
            k_proj: QLinear::from_linear(k_proj),
            v_proj: QLinear::from_linear(v_proj),
            dense: QLinear::from_linear(dense),
            q_layernorm,
            k_layernorm,
            rotary_emb: rope,
            softmax_scale,
            num_heads,
            num_kv_heads,
            head_dim,
            use_flash_attn: cfg.use_flash_attn,
            neg_inf: Tensor::new(f32::NEG_INFINITY, vb.device())?.to_dtype(vb.dtype())?,
        })
    }

    fn forward(
        &mut self,
        xs: &Tensor,
        mask: Option<&Tensor>,
        seqlen_offsets: &[usize],
        start_offsets_kernel: Tensor,
        kv_cache: &mut Option<(Tensor, Tensor)>,
    ) -> Result<Tensor> {
        let (b_size, seq_len, _n_embd) = xs.dims3()?;

        let original_dtype = xs.dtype();
        let mut xs = xs.clone();
        if self.q_proj.is_quant() {
            xs = xs.to_dtype(DType::F32)?;
        }
        let mut q = self.q_proj.forward(&xs)?;
        let mut k = self.k_proj.forward(&xs)?;
        let mut v = self.v_proj.forward(&xs)?;
        if self.q_proj.is_quant() {
            q = q.to_dtype(original_dtype)?;
            k = k.to_dtype(original_dtype)?;
            v = v.to_dtype(original_dtype)?;
        }

        let q = match &self.q_layernorm {
            None => q,
            Some(ln) => q.apply(ln)?,
        };
        let k = match &self.k_layernorm {
            None => k,
            Some(ln) => k.apply(ln)?,
        };

        let mut q = q.reshape((b_size * seq_len, self.num_heads, self.head_dim))?;
        let mut k = k.reshape((b_size * seq_len, self.num_kv_heads, self.head_dim))?;
        let v = v
            .reshape((b_size, seq_len, self.num_kv_heads, self.head_dim))?
            .transpose(1, 2)?;

        self.rotary_emb.forward(
            seqlen_offsets,
            &start_offsets_kernel,
            &mut q,
            &mut k,
            b_size,
        )?;

        if q.rank() == 3 {
            q = q
                .reshape((b_size, seq_len, self.num_heads, self.head_dim))?
                .transpose(1, 2)?
                .contiguous()?;
            k = k
                .reshape((b_size, seq_len, self.num_kv_heads, self.head_dim))?
                .transpose(1, 2)?
                .contiguous()?;
        }

        let (k, v) = match &*kv_cache {
            None => (k, v),
            Some((prev_k, prev_v)) => {
                let k = candle_nn::ops::kvconcat(prev_k, &k, 2)?;
                let v = candle_nn::ops::kvconcat(prev_v, &v, 2)?;
                (k, v)
            }
        };
        *kv_cache = Some((k.clone(), v.clone()));

        let k = repeat_kv(k, self.num_heads / self.num_kv_heads)?.contiguous()?;
        let v = repeat_kv(v, self.num_heads / self.num_kv_heads)?.contiguous()?;

        let mut attn_output = if self.use_flash_attn {
            // flash-attn expects (b_sz, seq_len, nheads, head_dim)
            let q = q.transpose(1, 2)?;
            let k = k.transpose(1, 2)?;
            let v = v.transpose(1, 2)?;
            flash_attn(&q, &k, &v, self.softmax_scale as f32, seq_len > 1)?.transpose(1, 2)?
        } else {
            let attn_weights = (q
                .to_dtype(DType::F32)?
                .contiguous()?
                .matmul(&k.to_dtype(DType::F32)?.t()?)?
                * self.softmax_scale)?;
            let attn_weights =
                CausalMasker.apply_mask(&mask.cloned(), attn_weights, &self.neg_inf)?;
            let attn_weights =
                candle_nn::ops::softmax_last_dim(&attn_weights)?.to_dtype(v.dtype())?;
            attn_weights.matmul(&v)?
        };
        if self.q_proj.is_quant() {
            attn_output = attn_output.to_dtype(DType::F32)?;
        }
        let mut res = attn_output
            .transpose(1, 2)?
            .reshape((b_size, seq_len, ()))?
            .apply(&self.dense)?;
        if self.q_proj.is_quant() {
            res = res.to_dtype(original_dtype)?;
        }
        Ok(res)
    }
}

#[derive(Clone)]
struct DecoderLayer {
    self_attn: Attention,
    mlp: MLP,
    input_layernorm: LayerNorm,
}

impl DecoderLayer {
    fn new(
        cfg: &Config,
        vb: VarBuilder,
        mapper: &dyn DeviceMapper,
        layer_idx: usize,
        loading_isq: bool,
        rotary_emb: RotaryEmbedding,
    ) -> Result<Self> {
        let self_attn = Attention::new(
            cfg,
            mapper.set_device(layer_idx, vb.pp("self_attn"), loading_isq),
            rotary_emb,
        )?;
        let mlp = MLP::new(cfg, mapper.set_device(layer_idx, vb.pp("mlp"), loading_isq))?;
        let input_layernorm = layer_norm(
            cfg.hidden_size,
            cfg.layer_norm_eps,
            mapper.set_device(layer_idx, vb.pp("input_layernorm"), false),
        )?;
        Ok(Self {
            self_attn,
            mlp,
            input_layernorm,
        })
    }

    fn forward(
        &mut self,
        xs: &Tensor,
        mask: Option<&Tensor>,
        seqlen_offsets: &[usize],
        start_offsets_kernel: Tensor,
        kv_cache: &mut Option<(Tensor, Tensor)>,
    ) -> Result<Tensor> {
        let residual = xs;
        let xs = xs.apply(&self.input_layernorm)?;
        let attn_outputs =
            self.self_attn
                .forward(&xs, mask, seqlen_offsets, start_offsets_kernel, kv_cache)?;
        let feed_forward_hidden_states = self.mlp.forward(&xs)?;
        attn_outputs + feed_forward_hidden_states + residual
    }
}

pub struct Model {
    embed_tokens: Embedding,
    layers: Vec<DecoderLayer>,
    final_layernorm: LayerNorm,
    lm_head: QLinear,
    pub cache: Cache,
    pub device: Device,
    pub max_seq_len: usize,
    mapper: Box<dyn DeviceMapper + Send + Sync>,
}

impl Model {
    pub fn new(
        cfg: &Config,
        vb: VarBuilder,
        is_gptx: bool,
        mapper: DeviceMapMetadata,
        loading_isq: bool,
        real_device: Device,
    ) -> Result<Self> {
        let vb_m = vb.pp("model");
        let mapper = mapper.into_mapper(cfg.num_hidden_layers, &real_device)?;
        let embed_tokens = embedding(
            cfg.vocab_size,
            cfg.hidden_size,
            mapper.set_nm_device(vb_m.pp("embed_tokens"), false),
        )?;
        let final_layernorm = layer_norm(
            cfg.hidden_size,
            cfg.layer_norm_eps,
            mapper.set_nm_device(vb_m.pp("final_layernorm"), false),
        )?;
        let mut layers = Vec::with_capacity(cfg.num_hidden_layers);
        let vb_m = vb_m.pp("layers");
        for layer_idx in 0..cfg.num_hidden_layers {
            // Alternative rope scalings are not supported.
            let rotary_emb = RotaryEmbedding::new_partial(
                cfg.rope_theta,
                cfg.head_dim(),
                (cfg.partial_rotary_factor * cfg.head_dim() as f64) as usize,
                cfg.max_position_embeddings,
                mapper.device_for(layer_idx, false).unwrap_or(&real_device),
                is_gptx,
                vb.dtype(),
            )?;
            let layer = DecoderLayer::new(
                cfg,
                vb_m.pp(layer_idx),
                &*mapper,
                layer_idx,
                loading_isq,
                rotary_emb,
            )?;
            layers.push(layer)
        }
        let lm_head = linear(
            cfg.hidden_size,
            cfg.vocab_size,
            mapper.set_nm_device(vb.pp("lm_head"), loading_isq),
        )?;
        Ok(Self {
            embed_tokens,
            layers,
            final_layernorm,
            lm_head: QLinear::from_linear(lm_head),
            cache: Cache::new(cfg.num_hidden_layers, false),
            device: real_device,
            max_seq_len: cfg.max_position_embeddings,
            mapper,
        })
    }

    pub fn forward(
        &mut self,
        xs: &Tensor,
        seqlen_offsets: &[usize],
        start_offsets_kernel: Tensor,
        context_lens: Vec<(usize, usize)>,
    ) -> Result<Tensor> {
        let mask = CausalMasker.make_causal_mask(xs, &self.cache)?;
        let mut xs = xs.apply(&self.embed_tokens)?;
        let mut cache = self.cache.lock();
        for (i, layer) in self.layers.iter_mut().enumerate() {
            xs = self.mapper.map(xs, i)?;
            xs = layer.forward(
                &xs,
                mask.as_ref()
                    .map(|m| m.to_device(xs.device()).unwrap())
                    .as_ref(),
                seqlen_offsets,
                start_offsets_kernel.clone(),
                &mut cache[i],
            )?;
        }
        let xs = xs.to_device(&self.device)?;
        let mut xs = xs.apply(&self.final_layernorm)?;
        if self.lm_head.is_quant() {
            xs = xs.to_dtype(DType::F32)?;
        }
        extract_logits(&xs.apply(&self.lm_head)?, context_lens)
    }
}

impl NormalModel for Model {
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
    fn cache(&self) -> &Cache {
        &self.cache
    }
    fn device(&self) -> &Device {
        &self.device
    }
    fn is_xlora(&self) -> bool {
        false
    }
    fn max_seq_len(&self) -> usize {
        self.max_seq_len
    }
    fn get_tensors(&mut self) -> (Vec<(&mut QMatMul, Option<usize>)>, &dyn DeviceMapper) {
        let mut tensors = Vec::new();
        tensors.push((self.lm_head.inner(), None));
        for (i, layer) in self.layers.iter_mut().enumerate() {
            tensors.push((layer.self_attn.q_proj.inner(), Some(i)));
            tensors.push((layer.self_attn.k_proj.inner(), Some(i)));
            tensors.push((layer.self_attn.v_proj.inner(), Some(i)));
            tensors.push((layer.self_attn.dense.inner(), Some(i)));
            tensors.push((layer.mlp.fc1.inner(), Some(i)));
            tensors.push((layer.mlp.fc2.inner(), Some(i)));
        }
        (tensors, &*self.mapper)
    }
}
