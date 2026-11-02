class A { m() {} }
class B { private m(a: number, b: number) {} n() {} }
function caller(x) { x.m(); }