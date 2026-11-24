__device__ float scale(float value) {
    return value * 2.0f;
}

__global__ void transform(float *data) {
    int index = threadIdx.x;
    data[index] = scale(data[index]);
}

void launch_transform(float *data, dim3 grid, dim3 block) {
    transform<<<grid, block>>>(data);
}
