class C {
 public void M() {}
 public void M(int value) {}
 public void Caller() { M(); M(1); }
}
