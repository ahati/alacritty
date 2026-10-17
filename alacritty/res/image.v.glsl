#if defined(GLES2_RENDERER)
attribute vec2 aPos;
attribute vec2 aUv;

// Texture coordinates are kept as precise as possible, since large images suffer
// from visible artifacts with lower precision.
varying highp vec2 vUv;
#else
layout (location = 0) in vec2 aPos;
layout (location = 1) in vec2 aUv;

out vec2 vUv;
#endif

void main() {
    vUv = aUv;

    // Positions are already in normalized device coordinates, with the origin in
    // the top-left corner of the window and Y pointing down.
    gl_Position = vec4(aPos.x, aPos.y, 0.0, 1.0);
}
