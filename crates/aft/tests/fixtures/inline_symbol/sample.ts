const BASE_URL = "https://api.example.com";

function add(a: number, b: number): number {
  const sum = a + b;
  return sum;
}

function main() {
  const x = 10;
  const y = 20;
  const result = add(x, y);
  console.log(result);
}

const double = (n: number): number => n * 2;

function caller() {
  const val = double(5);
  console.log(val);
}
