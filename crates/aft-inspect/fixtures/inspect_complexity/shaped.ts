export function shaped(flag: boolean, n: number): number {
  if (flag && n > 0) {
    for (let value = 0; value < n; value += 1) {
      void value;
    }
  }

  switch (n) {
    case 0:
      return 0;
    case 1:
      return 1;
    default:
      return flag ? n : -n;
  }
}
