#if defined(GLES2_RENDERER)
// Fragment shaders are only required to support `mediump` precision, so fall back
// to it when `highp` is unavailable, instead of failing to compile.
#if defined(GL_FRAGMENT_PRECISION_HIGH)
#define uv_t highp vec2
#else
#define uv_t mediump vec2
#endif

#define SAMPLE_TEXTURE texture2D
#define FRAG_COLOR gl_FragColor

varying uv_t vUv;
#else
#define uv_t vec2
#define SAMPLE_TEXTURE texture
#define FRAG_COLOR FragColor

in uv_t vUv;

out vec4 FragColor;
#endif

uniform sampler2D image;

void main() {
    // Images are uploaded with straight (unpremultiplied) alpha, so the texel is
    // passed through unmodified. The multiplication by the sampled alpha is done
    // by the caller's blend function (SRC_ALPHA, ONE_MINUS_SRC_ALPHA).
    FRAG_COLOR = SAMPLE_TEXTURE(image, vUv);
}
