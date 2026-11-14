#version 140
// `tex` holds premultiplied alpha (color scaled by alpha in linear light), so that mip levels
// and bilinear magnification blend transparent texels correctly. Opaque texels are unchanged.
uniform sampler2D tex;
uniform float bright_shade;
uniform float gamma;
uniform float lod_level;
uniform mat3 to_window_gamut;
in vec2 v_tex_coords;
out vec4 f_color;
void main() {
    vec4 color = textureLod(tex, v_tex_coords, lod_level);
    const float grid_size = 12.0;
    vec3 grid_color;
    if ((mod(gl_FragCoord.x, grid_size * 2.0) < grid_size)
        ^^ (mod(gl_FragCoord.y, grid_size * 2.0) < grid_size)
    ) {
        grid_color = vec3(bright_shade);
    } else {
        grid_color = vec3(bright_shade * 0.55);
    }
    // Straight color first, so the gamma curve doesn't act on premultiplied values. Then from
    // the gamut the texture is in to the window's (identity unless a Display P3 window shows
    // sRGB pixels); it happens in linear light, and an sRGB color never leaves the P3 range.
    vec3 straight_rgb = color.a > 0.0 ? color.rgb / color.a : vec3(0.0);
    straight_rgb = to_window_gamut * straight_rgb;
    vec3 image_rgb = pow(clamp(straight_rgb, 0.0, 1.0), vec3(1.0 / clamp(gamma, 0.25, 4.0)));
    // The picture is drawn without blending, so it composites onto the checkerboard itself and
    // the result is fully opaque.
    f_color = vec4(mix(grid_color, image_rgb, color.a), 1.0);
}
