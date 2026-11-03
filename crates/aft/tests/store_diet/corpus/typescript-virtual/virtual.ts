class A { m() {} }
class B extends A { f() { this.m(); super.m(); } }
class D extends B { m() {} }
function caller() { const x = new B(); x.m(); }