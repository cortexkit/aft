#include <metal_stdlib>
using namespace metal;

float brighten(float value) {
    return value + 1.0f;
}

kernel void brighten_buffer(device float *values [[buffer(0)]], uint id [[thread_position_in_grid]]) {
    values[id] = brighten(values[id]);
}
