function compute(x: number): number {
  const temp = x * 2;
  const result = temp + 10;
  return result;
}

function main() {
  const temp = 99;
  const result = compute(5);
  console.log(temp, result);
}
