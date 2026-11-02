<template>
  <div class="greeting">
    <h1>{{ message }}</h1>
    <button @click="handleClick">{{ buttonText }}</button>
  </div>
</template>

<script setup lang="ts">
import { ref, computed } from 'vue'

const message = ref('Hello, World!')
const buttonText = computed(() => 'Click me')

function handleClick() {
  console.log('clicked')
}
</script>

<style scoped>
.greeting {
  color: blue;
}
</style>
