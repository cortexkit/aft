// Test fixture: intra-file function calls for zoom command testing.

function helper(x: number): number {
  return x * 2;
}

function compute(a: number, b: number): number {
  const doubled = helper(a);
  return doubled + b;
}

function orchestrate(): number {
  const x = compute(1, 2);
  const y = helper(3);
  return x + y;
}

function unused(): void {
  console.log("nobody calls me");
}

class Calculator {
  add(a: number, b: number): number {
    return a + b;
  }

  runAll(): number {
    const sum = this.add(1, 2);
    const h = helper(sum);
    return h;
  }
}

const format = (n: number): string => {
  return n.toString();
};

function display(value: number): void {
  const text = format(value);
  console.log(text);
}
