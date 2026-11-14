use glium::{backend::Facade, program::ProgramCreationInput};

pub static VERTEX_140: &str = include_str!("shaders/vertex_140.glsl");
pub static TEXTURE_SHADOW_F_140: &str = include_str!("shaders/texture_shadow_f_140.glsl");
pub static COLOR_SHADOW_F_140: &str = include_str!("shaders/color_shadow_f_140.glsl");
pub static COLOR_F_140: &str = include_str!("shaders/color_f_140.glsl");

/// See [`glium::program::ProgramCreationInput::SourceCode`] for a
/// detailed description of these fields (note the `SourceCode` variant)
pub struct ShaderDescriptor<'a> {
	pub vertex_shader: &'a str,
	pub fragment_shader: &'a str,
	pub outputs_srgb: bool,
}
impl<'a> Default for ShaderDescriptor<'a> {
	fn default() -> Self {
		Self { vertex_shader: "", fragment_shader: "", outputs_srgb: true }
	}
}

pub fn shader_from_source<F: Facade>(
	facade: &F,
	desc: ShaderDescriptor,
) -> Result<glium::Program, glium::ProgramCreationError> {
	let input = ProgramCreationInput::SourceCode {
		vertex_shader: desc.vertex_shader,
		fragment_shader: desc.fragment_shader,
		tessellation_control_shader: None,
		tessellation_evaluation_shader: None,
		geometry_shader: None,
		transform_feedback_varyings: None,
		outputs_srgb: desc.outputs_srgb,
		uses_point_size: false,
	};
	glium::Program::new(facade, input)
}
