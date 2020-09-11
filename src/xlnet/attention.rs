// Copyright 2018 Google AI and Google Brain team.
// Copyright 2018 Carnegie Mellon University Authors.
// Copyright 2020-present, the HuggingFace Inc. team.
// Copyright 2020 Guillaume Becquin
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//     http://www.apache.org/licenses/LICENSE-2.0
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use crate::common::dropout::Dropout;
use crate::xlnet::XLNetConfig;
use std::borrow::Borrow;
use tch::nn::Init;
use tch::{nn, Kind, Tensor};

#[derive(Debug)]
/// # Cache for XLNet attention layers
/// Stores the cached value of the attention
pub struct LayerState {
    /// Cached content
    pub prev_content: Tensor,
}

#[derive(Debug)]
pub struct XLNetRelativeAttention {
    num_attention_heads: i64,
    attention_head_size: i64,
    hidden_size: i64,
    dropout: Dropout,
    output_attentions: bool,
    q: Tensor,
    k: Tensor,
    v: Tensor,
    o: Tensor,
    r: Tensor,
    r_r_bias: Tensor,
    r_s_bias: Tensor,
    r_w_bias: Tensor,
    seg_embed: Tensor,
    layer_norm: nn::LayerNorm,
    scale: f64,
}

impl XLNetRelativeAttention {
    pub fn new<'p, P>(p: P, config: &XLNetConfig) -> XLNetRelativeAttention
    where
        P: Borrow<nn::Path<'p>>,
    {
        assert_eq!(
            config.d_model % config.d_head,
            0,
            "Hidden size not a multiple of attention heads dimension"
        );
        let p = p.borrow();

        let q = p.var(
            "q",
            &[config.d_model, config.n_head, config.d_head],
            Init::KaimingUniform,
        );

        let k = p.var(
            "k",
            &[config.d_model, config.n_head, config.d_head],
            Init::KaimingUniform,
        );

        let v = p.var(
            "v",
            &[config.d_model, config.n_head, config.d_head],
            Init::KaimingUniform,
        );

        let o = p.var(
            "o",
            &[config.d_model, config.n_head, config.d_head],
            Init::KaimingUniform,
        );

        let r = p.var(
            "r",
            &[config.d_model, config.n_head, config.d_head],
            Init::KaimingUniform,
        );

        let r_r_bias = p.var(
            "r_r_bias",
            &[config.n_head, config.d_head],
            Init::KaimingUniform,
        );
        let r_s_bias = p.var(
            "r_s_bias",
            &[config.n_head, config.d_head],
            Init::KaimingUniform,
        );
        let r_w_bias = p.var(
            "r_w_bias",
            &[config.n_head, config.d_head],
            Init::KaimingUniform,
        );
        let seg_embed = p.var(
            "seg_embed",
            &[config.n_head, config.d_head],
            Init::KaimingUniform,
        );

        let dropout = Dropout::new(config.dropout);
        let output_attentions = match config.output_attentions {
            Some(value) => value,
            None => false,
        };
        let layer_norm_eps = match config.layer_norm_eps {
            Some(value) => value,
            None => 1e-12,
        };
        let layer_norm_config = nn::LayerNormConfig {
            eps: layer_norm_eps,
            ..Default::default()
        };
        let layer_norm = nn::layer_norm(p / "layer_norm", vec![config.d_model], layer_norm_config);

        let scale = 1f64 / ((config.d_head as f64).powf(0.5f64));

        XLNetRelativeAttention {
            num_attention_heads: config.n_head,
            attention_head_size: config.d_head,
            hidden_size: config.d_model,
            dropout,
            output_attentions,
            q,
            k,
            v,
            o,
            r,
            r_r_bias,
            r_s_bias,
            r_w_bias,
            seg_embed,
            layer_norm,
            scale,
        }
    }

    fn rel_shift(&self, x: &Tensor, klen: i64) -> Tensor {
        let shape = x.size();
        x.reshape(&[shape[1], shape[0], shape[2], shape[3]])
            .narrow(0, 1, shape[1] - 1)
            .reshape(&[shape[0], shape[1] - 1, shape[2], shape[3]])
            .index_select(1, &Tensor::arange(klen, (Kind::Int64, x.device())))
    }

    fn rel_shift_bnij(&self, x: &Tensor, klen: i64) -> Tensor {
        let shape = x.size();
        x.reshape(&[shape[0], shape[1], shape[3], shape[2]])
            .narrow(2, 1, shape[1] - 1)
            .reshape(&[shape[0], shape[1], shape[2], shape[3] - 1])
            .index_select(1, &Tensor::arange(klen, (Kind::Int64, x.device())))
    }

    fn rel_attention_core(
        &self,
        q_head: &Tensor,
        k_head_h: &Tensor,
        v_head_h: &Tensor,
        k_head_r: &Tensor,
        seg_mat: Option<&Tensor>,
        attention_mask: Option<&Tensor>,
        train: bool,
    ) -> (Tensor, Option<Tensor>) {
        let ac = Tensor::einsum("ibnd,jbnd->bnij", &[&(q_head + &self.r_w_bias), &k_head_h]);
        let bd = self.rel_shift_bnij(
            &Tensor::einsum("ibnd,jbnd->bnij", &[&(q_head + &self.r_r_bias), &k_head_r]),
            ac.size()[3],
        );

        let ef = match seg_mat {
            Some(seg_mat) => {
                let ef = Tensor::einsum(
                    "ibnd,snd->ibns",
                    &[&(q_head + &self.r_s_bias), &self.seg_embed],
                );
                Tensor::einsum("ijbs,ibns->bnij", &[seg_mat, &ef])
            }
            None => Tensor::zeros(&[1], (Kind::Float, ac.device())),
        };

        let mut attention_score = (ac + bd + ef) * self.scale;
        if let Some(value) = attention_mask {
            attention_score = attention_score - value.permute(&[2, 3, 0, 1]) * 1e30;
        };

        let attention_probas = attention_score
            .softmax(3, Kind::Float)
            .apply_t(&self.dropout, train);

        let attention_vector = Tensor::einsum("bnij,jbnd->ibnd", &[&attention_probas, v_head_h]);

        if self.output_attentions {
            (
                attention_vector,
                Some(attention_probas.permute(&[2, 3, 0, 1])),
            )
        } else {
            (attention_vector, None)
        }
    }

    fn post_attention(
        &self,
        h: &Tensor,
        attention_vector: &Tensor,
        residual: bool,
        train: bool,
    ) -> Tensor {
        let mut attention_out = Tensor::einsum("ibnd,hnd->ibh", &[attention_vector, &self.o])
            .apply_t(&self.dropout, train);
        if residual {
            attention_out = attention_out + h;
        };
        attention_out.apply(&self.layer_norm)
    }

    pub fn forward_t(
        &self,
        h: &Tensor,
        g: Option<&Tensor>,
        attn_mask_h: Option<&Tensor>,
        attn_mask_g: Option<&Tensor>,
        r: &Tensor,
        seg_mat: Option<&Tensor>,
        mut layer_state: Option<LayerState>,
        target_mapping: Option<&Tensor>,
        train: bool,
    ) -> (Tensor, Option<Tensor>, Option<Tensor>, Option<Tensor>) {
        if g.is_some() {
            let cat_value = if let Some(mems) = &layer_state {
                if mems.prev_content.size().len() > 1 {
                    Some(Tensor::cat(&[&mems.prev_content, h], 0))
                } else {
                    None
                }
            } else {
                None
            };
            let cat = match &cat_value {
                Some(value) => value,
                None => h,
            };

            let q_head_h = Tensor::einsum("ibh,hnd->ibnd", &[h, &self.q]);
            let k_head_h = Tensor::einsum("ibh,hnd->ibnd", &[cat, &self.k]);
            let v_head_h = Tensor::einsum("ibh,hnd->ibnd", &[cat, &self.v]);
            let k_head_r = Tensor::einsum("ibh,hnd->ibnd", &[r, &self.r]);

            let (attention_vec_h, attention_probas_h) = self.rel_attention_core(
                &q_head_h,
                &k_head_h,
                &v_head_h,
                &k_head_r,
                seg_mat,
                attn_mask_h,
                train,
            );

            let output_h = self.post_attention(h, &attention_vec_h, true, train);
            let q_head_g = Tensor::einsum("ibh,hnd->ibnd", &[g.unwrap(), &self.q]);

            let (attention_vec_g, attention_probas_g) = match target_mapping {
                Some(target_mapping) => {
                    let q_head_g = Tensor::einsum("mbnd,mlb->lbnd", &[&q_head_g, target_mapping]);
                    let (attention_vec_g, attention_probas_g) = self.rel_attention_core(
                        &q_head_g,
                        &k_head_h,
                        &v_head_h,
                        &k_head_r,
                        seg_mat,
                        attn_mask_g,
                        train,
                    );
                    let attention_vec_g =
                        Tensor::einsum("lbnd,mlb->mbnd", &[&attention_vec_g, target_mapping]);
                    (attention_vec_g, attention_probas_g)
                }
                None => self.rel_attention_core(
                    &q_head_g,
                    &k_head_h,
                    &v_head_h,
                    &k_head_r,
                    seg_mat,
                    attn_mask_g,
                    train,
                ),
            };

            let output_g = self.post_attention(g.unwrap(), &attention_vec_g, true, train);
            (
                output_h,
                Some(output_g),
                attention_probas_h,
                attention_probas_g,
            )
        } else {
            let cat_value = if let Some(mems) = &layer_state {
                if mems.prev_content.size().len() > 1 {
                    Some(Tensor::cat(&[&mems.prev_content, h], 0))
                } else {
                    None
                }
            } else {
                None
            };
            let cat = match &cat_value {
                Some(value) => value,
                None => h,
            };

            let q_head_h = Tensor::einsum("ibh,hnd->ibnd", &[h, &self.q]);
            let k_head_h = Tensor::einsum("ibh,hnd->ibnd", &[cat, &self.k]);
            let v_head_h = Tensor::einsum("ibh,hnd->ibnd", &[cat, &self.v]);
            let k_head_r = Tensor::einsum("ibh,hnd->ibnd", &[r, &self.r]);

            let (attention_vec, attention_probas) = self.rel_attention_core(
                &q_head_h,
                &k_head_h,
                &v_head_h,
                &k_head_r,
                seg_mat,
                attn_mask_h,
                train,
            );

            let output_h = self.post_attention(h, &attention_vec, true, train);
            (output_h, None, attention_probas, None)
        }
    }
}
