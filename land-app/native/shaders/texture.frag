#version 450

layout(location = 0) in vec2 frag_uv;
layout(location = 0) out vec4 out_color;

layout(binding = 0) uniform sampler2D imported_texture;

void main() {
    out_color = texture(imported_texture, frag_uv);
}
