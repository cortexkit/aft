class C {
 fun m() {}
 fun f(x: C) {
 this.m()
 x.m()
 val y: C = x
 y.m()
 }
}