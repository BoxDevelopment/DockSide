// Portions from the Windhawk mod "MacOS Minimize Animation" (MIT, Abdullah Masood and Potassiumuncher).

// Genie renderer. The genie styles come from the "MacOS Minimize Animation"
// Windhawk mod (MIT): the modern lamp curve is Potassiumuncher's CalculateLampVertexMacOS,
// the classic one the mod's original strip warp; the scale effect is our own. All
// are evaluated per vertex on the GPU instead of building ~1200 path geometries (or a
// per-scanline copy) per frame on the CPU. The window's shadow is a second pass over a
// grid that reaches past the window, warped by the same curve.

cbuffer GenieCB : register(b0)
{
    float4 gWin;      // window rect: x, y, width, height (screen px)
    float4 gIcon;     // modern: taskbar icon rect x, y, width, height (screen px)
                      // classic: dock x, dock y, neck width, 1 = dock above the window
                      // scale: the rect the window ends in
                      // shatter: columns, rows, seed (the cracks differ every time),
                      // 1 = fragments (a few big pieces, each its own way)
    float4 gCanvas;   // ghost canvas: left, top, width, height (screen px)
    float4 gParams;   // x = progress t, y = corner radius (snapshot px), z = tiles per side,
                      // w = global opacity
    float4 gTex;      // snapshot width, height (px), corner extent (px), corner exponent
    float4 gShadow;   // x = how far the grid reaches past the window (snapshot px; 0 for
                      // the window itself), y, z = broad shadow sigma, offset (snapshot px)
    float4 gShadow2;  // x, y = contact shadow sigma, offset (snapshot px), z, w = opacities
};

cbuffer PrepCB : register(b1)
{
    int4 gPrep;       // x, y = crop offset in the raw capture (px), z = force opaque alpha
};

Texture2D gSnapshot : register(t0);
Texture2D<float4> gRaw : register(t0);
SamplerState gSampler : register(s0);

static const float PI = 3.14159265;
static const float CLASSIC_SPREAD = 0.65;   // rows nearest the dock lead the morph

// Maps a mesh vertex (tx, ty in 0..1) at progress p to its warped screen position.
float2 LampVertexMacOS(float tx, float ty, float p)
{
    float split = 0.3;
    float k = (p <= split) ? (p / split) : 1.0;
    float j = (p > split) ? ((p - split) / (1.0 - split)) : 0.0;

    float expandHeight = (gIcon.y - gWin.y - gWin.w);
    float fullHeight = (gIcon.y - gWin.y) - (expandHeight * (1.0 - k));
    float height = fullHeight - (j * fullHeight);
    float safeFull = (abs(fullHeight) < 0.001) ? 0.001 : fullHeight;

    float y = ty * height;
    float x = tx * gIcon.z
            + tx * (gWin.z - gIcon.z) * (1.0 - j) * (1.0 - ty)
            + tx * (gWin.z - gIcon.z) * (1.0 - k) * ty;

    float offsetX = (gIcon.x - gWin.x) * (y / (fullHeight + 0.1)) * k + (gIcon.x - gWin.x) * j;
    float offsetY = gIcon.y - gWin.y - height - (expandHeight * (1.0 - k));

    float effectX = sin(((height - y) / safeFull) * 2.0 * PI + PI)
                  * (gWin.x + gWin.z * tx - (gIcon.x + gIcon.z * tx)) / 7.0 * k;

    return float2(gWin.x + x + offsetX + effectX, gWin.y + y + offsetY);
}

struct GenieVSOut
{
    float4 pos : SV_Position;
    float2 uv : TEXCOORD0;
};

// No vertex buffer: the (tiles+1)^2 grid vertices are derived from SV_VertexID. The
// shadow pass stretches the grid gShadow.x px past every edge of the window.
float2 GridUV(uint id)
{
    uint tiles = (uint)gParams.z;
    float2 g = float2(id % (tiles + 1), id / (tiles + 1)) / gParams.z;
    float2 margin = gShadow.x / gTex.xy;
    return g * (1.0 + 2.0 * margin) - margin;
}

GenieVSOut Output(float2 s, float2 uv)
{
    float2 c = (s - gCanvas.xy) / gCanvas.zw;
    GenieVSOut o;
    o.pos = float4(c.x * 2.0 - 1.0, 1.0 - c.y * 2.0, 0.0, 1.0);
    o.uv = uv;
    return o;
}

GenieVSOut GenieVS(uint id : SV_VertexID)
{
    float2 uv = GridUV(id);
    return Output(LampVertexMacOS(uv.x, uv.y, gParams.x), uv);
}

// Scale: the window shrinks into its end rect (gIcon). The size leads and both settle
// together (ScaleVertex in genie_effect.cpp).
GenieVSOut ScaleVS(uint id : SV_VertexID)
{
    float2 uv = GridUV(id);
    float t = gParams.x;
    float size = 1.0 - (1.0 - t) * (1.0 - t);
    float pos = t * t * (3.0 - 2.0 * t);
    float2 from = gWin.xy + gWin.zw * 0.5;
    float2 to = gIcon.xy + gIcon.zw * 0.5;
    float2 dim = gWin.zw + (gIcon.zw - gWin.zw) * size;
    return Output(from + (to - from) * pos + (uv - 0.5) * dim, uv);
}

float ClassicMorph(float v, float tt)
{
    float m = saturate(tt * (1.0 + CLASSIC_SPREAD) - (1.0 - v) * CLASSIC_SPREAD);
    return m * m * (3.0 - 2.0 * m);
}

// Classic: every source row slides towards the dock edge and narrows to the neck.
GenieVSOut ClassicVS(uint id : SV_VertexID)
{
    float2 uv = GridUV(id);
    float e = ClassicMorph(gIcon.w > 0.5 ? 1.0 - uv.y : uv.y, gParams.x);
    float idY = gWin.y + gWin.w * uv.y;
    float y = idY + (gIcon.y - idY) * e;
    float width = gWin.z + (gIcon.z - gWin.z) * e;
    float cx0 = gWin.x + gWin.z * 0.5;
    float cx = cx0 + (gIcon.x - cx0) * e;
    return Output(float2(cx + (uv.x - 0.5) * width, y), uv);
}

// Signed distance (snapshot px, < 0 inside) to the window's shape: a superellipse
// |x|^n + |y|^n = 1 spanning gTex.z px from each corner (see common/corner_shape.h).
// n = 2 is Windows 11's circular corner; larger n gives the smoothed macOS squircle.
float WindowDistance(float2 uv)
{
    float2 halfSize = gTex.xy * 0.5;
    float2 qe = abs(uv * gTex.xy - halfSize) - halfSize;   // <= 0 inside the window
    float p = gTex.z;
    float2 k = max(qe + p, 0.0);                           // depth into a corner box
    if (p > 0.0 && (k.x > 0.0 || k.y > 0.0))
    {
        // Distance to the curve, to first order: (m - 1) / |grad m|, m = ||k / p||_n.
        float n = gTex.w;
        float2 kn = k / p;
        float2 kp = pow(kn, n);
        float m = pow(kp.x + kp.y, 1.0 / n);
        float2 grad = (kp / max(kn, 1e-6)) / max(pow(m, n - 1.0), 1e-6);
        return (m - 1.0) * p / max(length(grad), 1e-3);
    }
    return max(qe.x, qe.y);
}

// Premultiplied snapshot (mipmapped, anisotropic) cut to the window's shape.
float4 WindowColor(float2 uv)
{
    float4 c = gSnapshot.Sample(gSampler, uv);
    if (gTex.z > 0.0)
    {
        float d = WindowDistance(uv);
        float aa = max(fwidth(d), 0.001);
        c *= saturate(0.5 - d / aa);
    }
    return c;
}

float4 GeniePS(GenieVSOut i) : SV_Target
{
    return WindowColor(i.uv) * gParams.w;
}

// ---- Shatter and fragments (our own) ---------------------------------------------------
// The window cracks along a jittered grid into triangular shards that burst outwards from
// its middle, spinning, shrinking and fading as they slow down; the outer ones come loose
// first and fly farthest. Shatter: many shards in four streams (left, right, up, down).
// Fragments: a few big pieces, each flying its own way. Opening runs it backwards (t from
// 1 to 0): the pieces fly together into the window. Mirrored in GenieMath.ShatterTriangle
// (settings-app), which draws the live preview.

uint ShatterHash(uint x)
{
    x ^= x >> 16;
    x *= 0x7feb352du;
    x ^= x >> 15;
    x *= 0x846ca68bu;
    x ^= x >> 16;
    return x;
}

// 0..1, the same for the same (item, salt) during one close.
float ShatterRand(uint item, uint salt)
{
    return (ShatterHash(item * 0x9E3779B1u + salt * 0x85EBCA77u + (uint)gIcon.z) & 0xFFFFFFu) / 16777216.0;
}

// A corner of the crack grid in window uv. Inner points are jittered (by less than half a
// cell, so cells never fold over); points on the outline only slide along it, which keeps
// the window's edges straight.
float2 ShatterPoint(uint2 p, uint2 n)
{
    uint id = p.y * (n.x + 1) + p.x;
    float2 j = float2(ShatterRand(id, 1), ShatterRand(id, 2)) - 0.5;
    if (p.x == 0 || p.x == n.x) j.x = 0.0;
    if (p.y == 0 || p.y == n.y) j.y = 0.0;
    return (float2(p) + j * 0.72) / float2(n);
}

struct ShardVSOut
{
    float4 pos : SV_Position;
    float2 uv : TEXCOORD0;
    float2 look : TEXCOORD1;   // x = opacity, y = light (a shard turning away gets darker)
};

// Two triangles per grid cell, three vertices each, no vertex or index buffer.
ShardVSOut ShatterVS(uint id : SV_VertexID)
{
    uint2 n = uint2(gIcon.xy);
    uint tri = id / 3;
    uint corner = id % 3;
    uint cell = tri / 2;
    uint2 c = uint2(cell % n.x, cell / n.x);
    // The cell's corners: 0 top left, 1 top right, 2 bottom left, 3 bottom right; the
    // diagonal that splits it goes either way.
    bool flip = ShatterRand(cell, 3) > 0.5;
    uint3 k = (tri % 2 == 0) ? (flip ? uint3(0, 1, 3) : uint3(0, 1, 2))
                             : (flip ? uint3(0, 3, 2) : uint3(1, 3, 2));
    float2 q[4] = {ShatterPoint(c, n), ShatterPoint(c + uint2(1, 0), n), ShatterPoint(c + uint2(0, 1), n),
                   ShatterPoint(c + uint2(1, 1), n)};
    float2 a = q[k.x], b = q[k.y], d = q[k.z];
    float2 uv = corner == 0 ? a : (corner == 1 ? b : d);
    float2 centre = (a + b + d) / 3.0;

    float2 rel = (centre - 0.5) * gWin.zw;                        // px from the window's middle
    float reach = saturate(length(centre - 0.5) * 1.41421356);    // 0 middle .. 1 corner
    bool big = gIcon.w > 0.5;
    // Outwards. Shatter: pulled most of the way towards the nearest of left, right, up and
    // down - four streams of shards. Fragments: every piece its own way.
    float angle = atan2(rel.y, rel.x);
    float axis = round(angle / (PI * 0.5)) * (PI * 0.5);
    angle = big ? angle + (ShatterRand(tri, 4) - 0.5) * 1.1
                : lerp(angle, axis, 0.7) + (ShatterRand(tri, 4) - 0.5) * 0.35;
    float2 dir = float2(cos(angle), sin(angle));

    // First the window only cracks (the pieces part a little), then they burst away and
    // slow down; the outer ones come loose first.
    float delay = big ? 0.05 + 0.06 * (1.0 - reach) + 0.08 * ShatterRand(tri, 5)
                      : 0.05 + 0.1 * (1.0 - reach) + 0.05 * ShatterRand(tri, 5);
    float t = gParams.x;
    float crack = smoothstep(0.0, delay, t);
    float u = saturate((t - delay) / (1.0 - delay));
    float move = 1.0 - pow(1.0 - u, 2.6);
    float diag = length(gWin.zw);
    float fly = big ? (0.2 + 0.24 * reach) * (0.7 + 0.6 * ShatterRand(tri, 6))
                    : (0.12 + 0.22 * reach) * (0.75 + 0.5 * ShatterRand(tri, 6));
    float dist = diag * (0.012 * crack + fly * move);
    // Big pieces turn slower and keep more of their size.
    float spin = (ShatterRand(tri, 7) * 2.0 - 1.0) * (0.03 * crack + (big ? 0.8 : 1.5) * move);
    float shrink = 1.0 - (big ? 0.25 : 0.4) * move;

    float2 v = (uv - centre) * gWin.zw * shrink;
    float sn = sin(spin), cs = cos(spin);
    v = float2(v.x * cs - v.y * sn, v.x * sn + v.y * cs);
    float2 s = gWin.xy + centre * gWin.zw + v + dir * dist;

    float2 cpos = (s - gCanvas.xy) / gCanvas.zw;
    ShardVSOut o;
    o.pos = float4(cpos.x * 2.0 - 1.0, 1.0 - cpos.y * 2.0, 0.0, 1.0);
    o.uv = uv;
    o.look = float2(1.0 - smoothstep(0.35, 1.0, u), 1.0 - 0.28 * move * ShatterRand(tri, 8));
    return o;
}

float4 ShatterPS(ShardVSOut i) : SV_Target
{
    float4 c = WindowColor(i.uv);
    c.rgb *= i.look.y;
    return c * (i.look.x * gParams.w);
}

// Exact distance to a plain rounded rectangle around 0: the shadow's shape (blurred,
// the squircle's difference does not show).
float RoundRectDistance(float2 p, float2 halfSize, float r)
{
    float2 q = abs(p) - halfSize + r;
    return length(max(q, 0.0)) + min(max(q.x, q.y), 0.0) - r;
}

// erfc to ~1e-4 (Winitzki's approximation of erf).
float Erfc(float x)
{
    float x2 = x * x;
    float a = 0.147;
    float erfAbs = sqrt(1.0 - exp(-x2 * (1.27323954 + a * x2) / (1.0 + a * x2)));
    return 1.0 - sign(x) * erfAbs;
}

// The window's shadow (mw::WindowShadow, as the shade of a window cut to its corners
// draws it): a broad and a contact layer, nothing under the window itself.
float4 ShadowPS(GenieVSOut i) : SV_Target
{
    float2 halfSize = gTex.xy * 0.5;
    float2 p = i.uv * gTex.xy - halfSize;
    float r = min(gParams.y, min(halfSize.x, halfSize.y));
    float cover = saturate(0.5 - WindowDistance(i.uv));
    float s1 = gShadow2.z * 0.5 * Erfc(RoundRectDistance(p - float2(0.0, gShadow.z), halfSize, r) / (gShadow.y * 1.41421356));
    float s2 = gShadow2.w * 0.5 * Erfc(RoundRectDistance(p - float2(0.0, gShadow2.y), halfSize, r) / (gShadow2.x * 1.41421356));
    float a = (1.0 - (1.0 - s1) * (1.0 - s2)) * (1.0 - cover);
    return float4(0.0, 0.0, 0.0, a * gParams.w);
}

// One-off pass per snapshot: crop the raw capture to the extended frame and fix its
// alpha, then mipmaps are generated from the result.
float4 FullscreenVS(uint id : SV_VertexID) : SV_Position
{
    float2 uv = float2((id << 1) & 2, id & 2);
    return float4(uv * float2(2.0, -2.0) + float2(-1.0, 1.0), 0.0, 1.0);
}

float4 PrepPS(float4 pos : SV_Position) : SV_Target
{
    float4 c = gRaw.Load(int3(int2(pos.xy) + gPrep.xy, 0));
    // The mod's fallback: when most of the window came back fully transparent the
    // alpha is not describing real translucency (GDI apps, Mica on secondary
    // monitors), so those pixels are made opaque instead of becoming a hole.
    if (gPrep.z != 0 && c.a == 0.0) c.a = 1.0;
    // Keep the premultiplied invariant: PrintWindow leaves stray colour under alpha 0.
    c.rgb = min(c.rgb, c.aaa);
    return c;
}
