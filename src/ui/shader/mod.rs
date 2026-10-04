//! Custom iced/wgpu shaders for immersive atmosphere.

pub mod ambient;
pub mod vignette;

pub use ambient::{AmbientProgram, AmbientUniforms};
pub use vignette::{VignetteProgram, VignetteUniforms};
