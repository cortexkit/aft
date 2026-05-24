function multiReturn(x: number): number {
  if (x > 0) {
    return x * 2;
  }
  return x * -1;
}

function caller() {
  const result = multiReturn(5);
  console.log(result);
}
