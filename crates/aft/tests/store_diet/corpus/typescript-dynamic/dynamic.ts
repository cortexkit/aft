class A { m() {} }
class B { m() {} n() {} }
function caller(x, name) { x.m(); x[name](); }