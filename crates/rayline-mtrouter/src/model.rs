use std::collections::HashMap;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result, anyhow};
use safetensors::{Dtype, SafeTensors};

use crate::manifest::Manifest;

#[derive(Clone, Debug)]
struct Tensor {
    shape: Vec<usize>,
    data: Vec<f32>,
}

#[derive(Clone, Debug)]
pub struct Estimator {
    tensors: HashMap<String, Tensor>,
    arm_embeddings: Vec<Vec<f32>>,
}

impl Estimator {
    pub fn load(runtime_dir: &Path, manifest: &Manifest) -> Result<Self> {
        let weights_path = runtime_dir.join(&manifest.weights.file);
        let bytes = fs::read(&weights_path)
            .with_context(|| format!("read C82 weights {}", weights_path.display()))?;
        let safe = SafeTensors::deserialize(&bytes)?;
        let mut tensors = HashMap::new();
        for name in required_tensor_names() {
            let view = safe
                .tensor(name)
                .with_context(|| format!("C82 weights missing tensor {name}"))?;
            if view.dtype() != Dtype::F32 {
                return Err(anyhow!("C82 tensor {name} must be F32"));
            }
            let data = view
                .data()
                .chunks_exact(4)
                .map(|chunk| f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
                .collect::<Vec<_>>();
            tensors.insert(
                (*name).to_owned(),
                Tensor {
                    shape: view.shape().to_vec(),
                    data,
                },
            );
        }
        let mut estimator = Self {
            tensors,
            arm_embeddings: Vec::new(),
        };
        estimator.validate_shapes()?;
        estimator.arm_embeddings = (0..manifest.workers.len())
            .map(|index| estimator.arm_embedding(index))
            .collect::<Result<Vec<_>>>()?;
        Ok(estimator)
    }

    pub fn q_values(
        &self,
        history: &[f32],
        previous_arm: Option<usize>,
        turn_index: u64,
    ) -> Result<Vec<f32>> {
        if history.len() != 1024 {
            return Err(anyhow!(
                "C82 history embedding has {} dimensions; expected 1024",
                history.len()
            ));
        }
        if history.iter().any(|value| !value.is_finite()) {
            return Err(anyhow!("C82 history embedding contains a non-finite value"));
        }
        if previous_arm.is_some_and(|index| index >= self.arm_embeddings.len()) {
            return Err(anyhow!("C82 previous arm index is out of range"));
        }
        let mut normalized = history.to_vec();
        normalize(&mut normalized)?;
        let previous = previous_arm
            .map(|index| self.arm_embeddings[index].as_slice())
            .unwrap_or(&[]);
        let mut result = Vec::with_capacity(self.arm_embeddings.len());
        for (index, candidate) in self.arm_embeddings.iter().enumerate() {
            let mut input = Vec::with_capacity(1154);
            input.extend_from_slice(&normalized);
            input.extend_from_slice(candidate);
            if previous.is_empty() {
                input.extend(std::iter::repeat_n(0.0, 64));
            } else {
                input.extend_from_slice(previous);
            }
            input.push(f32::from(previous_arm == Some(index)));
            input.push((turn_index as f32).ln_1p());
            let mut hidden = self.linear("q_network.backbone.0", &input)?;
            relu(&mut hidden);
            hidden = self.linear("q_network.backbone.3", &hidden)?;
            relu(&mut hidden);
            result.push(self.linear("q_network.head", &hidden)?[0]);
        }
        Ok(result)
    }

    fn arm_embedding(&self, index: usize) -> Result<Vec<f32>> {
        let meta = row(self.tensor("model_encoder.all_metas")?, index)?;
        let mut meta_embed = self.linear("model_encoder.meta_mlp.0", meta)?;
        relu(&mut meta_embed);
        meta_embed = self.linear("model_encoder.meta_mlp.2", &meta_embed)?;
        let residual = row(self.tensor("model_encoder.residual_embed.weight")?, index)?;
        meta_embed.extend_from_slice(residual);
        let mut output = self.linear("model_encoder.output_proj.0", &meta_embed)?;
        self.layer_norm("model_encoder.output_proj.1", &mut output)?;
        Ok(output)
    }

    fn validate_shapes(&self) -> Result<()> {
        for (name, shape) in [
            ("model_encoder.meta_mean", &[8][..]),
            ("model_encoder.meta_std", &[8][..]),
            ("model_encoder.all_metas", &[7, 8][..]),
            ("model_encoder.meta_mlp.0.weight", &[32, 8][..]),
            ("model_encoder.meta_mlp.0.bias", &[32][..]),
            ("model_encoder.meta_mlp.2.weight", &[32, 32][..]),
            ("model_encoder.meta_mlp.2.bias", &[32][..]),
            ("model_encoder.residual_embed.weight", &[7, 16][..]),
            ("model_encoder.output_proj.0.weight", &[64, 48][..]),
            ("model_encoder.output_proj.0.bias", &[64][..]),
            ("model_encoder.output_proj.1.weight", &[64][..]),
            ("model_encoder.output_proj.1.bias", &[64][..]),
            ("q_network.backbone.0.weight", &[256, 1154][..]),
            ("q_network.backbone.0.bias", &[256][..]),
            ("q_network.backbone.3.weight", &[256, 256][..]),
            ("q_network.backbone.3.bias", &[256][..]),
            ("q_network.head.weight", &[1, 256][..]),
            ("q_network.head.bias", &[1][..]),
        ] {
            require_shape(self.tensor(name)?, shape)
                .with_context(|| format!("validate C82 tensor {name}"))?;
        }
        Ok(())
    }

    fn tensor(&self, name: &str) -> Result<&Tensor> {
        self.tensors
            .get(name)
            .ok_or_else(|| anyhow!("missing C82 tensor {name}"))
    }

    fn linear(&self, prefix: &str, input: &[f32]) -> Result<Vec<f32>> {
        let weight = self.tensor(&format!("{prefix}.weight"))?;
        let bias = self.tensor(&format!("{prefix}.bias"))?;
        if weight.shape.len() != 2 || weight.shape[1] != input.len() {
            return Err(anyhow!(
                "{prefix} expected {} inputs, got {}",
                weight.shape.get(1).copied().unwrap_or(0),
                input.len()
            ));
        }
        require_shape(bias, &[weight.shape[0]])?;
        Ok((0..weight.shape[0])
            .map(|output| {
                weight.data[output * input.len()..(output + 1) * input.len()]
                    .iter()
                    .zip(input)
                    .fold(bias.data[output], |sum, (weight, value)| {
                        weight.mul_add(*value, sum)
                    })
            })
            .collect())
    }

    fn layer_norm(&self, prefix: &str, values: &mut [f32]) -> Result<()> {
        let weight = self.tensor(&format!("{prefix}.weight"))?;
        let bias = self.tensor(&format!("{prefix}.bias"))?;
        require_shape(weight, &[values.len()])?;
        require_shape(bias, &[values.len()])?;
        let mean = values.iter().sum::<f32>() / values.len() as f32;
        let variance = values
            .iter()
            .map(|value| (value - mean).powi(2))
            .sum::<f32>()
            / values.len() as f32;
        let denominator = (variance + 1e-5).sqrt();
        for (index, value) in values.iter_mut().enumerate() {
            *value = ((*value - mean) / denominator) * weight.data[index] + bias.data[index];
        }
        Ok(())
    }
}

fn normalize(values: &mut [f32]) -> Result<()> {
    let norm = values.iter().map(|value| value * value).sum::<f32>().sqrt();
    if !norm.is_finite() || norm <= 0.0 {
        return Err(anyhow!(
            "C82 history embedding norm must be finite and positive"
        ));
    }
    for value in values {
        *value /= norm;
    }
    Ok(())
}

fn row(tensor: &Tensor, index: usize) -> Result<&[f32]> {
    if tensor.shape.len() != 2 || index >= tensor.shape[0] {
        return Err(anyhow!("invalid row {index} for shape {:?}", tensor.shape));
    }
    let width = tensor.shape[1];
    Ok(&tensor.data[index * width..(index + 1) * width])
}

fn require_shape(tensor: &Tensor, expected: &[usize]) -> Result<()> {
    if tensor.shape != expected {
        return Err(anyhow!(
            "tensor has shape {:?}; expected {:?}",
            tensor.shape,
            expected
        ));
    }
    Ok(())
}

fn relu(values: &mut [f32]) {
    for value in values {
        *value = value.max(0.0);
    }
}

fn required_tensor_names() -> &'static [&'static str] {
    &[
        "model_encoder.meta_mean",
        "model_encoder.meta_std",
        "model_encoder.all_metas",
        "model_encoder.meta_mlp.0.weight",
        "model_encoder.meta_mlp.0.bias",
        "model_encoder.meta_mlp.2.weight",
        "model_encoder.meta_mlp.2.bias",
        "model_encoder.residual_embed.weight",
        "model_encoder.output_proj.0.weight",
        "model_encoder.output_proj.0.bias",
        "model_encoder.output_proj.1.weight",
        "model_encoder.output_proj.1.bias",
        "q_network.backbone.0.weight",
        "q_network.backbone.0.bias",
        "q_network.backbone.3.weight",
        "q_network.backbone.3.bias",
        "q_network.head.weight",
        "q_network.head.bias",
    ]
}
