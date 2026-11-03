interface I { m(): void; }
class A implements I { m() {} }
class B implements I { m() {} }
function caller(x: I) { x.m(); }