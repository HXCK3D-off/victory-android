struct Uniforms
{
    float4 tint;
};
ConstantBuffer<Uniforms> u : register(b0);

struct VsOut
{
    float4 pos : SV_Position;
    float2 uv : TEXCOORD0;
};

VsOut vs_main(uint index : SV_VertexID)
{
    float2 p = float2(float((index << 1) & 2), float(index & 2));
    VsOut o;
    o.pos = float4(p * 2.0 - 1.0, 0.0, 1.0);
    o.uv = p;
    return o;
}

float4 ps_main(VsOut i) : SV_Target
{
    float d = distance(i.uv, float2(0.5, 0.5));
    float glow = 1.0 - smoothstep(0.02, 0.8, d);
    return float4(u.tint.rgb * glow, 1.0);
}
