class C:
 def m(self): pass
 def f(self, x: C):
  self.m()
  x.m()
  y: C
  y.m()
  z = C()
  z.m()
 @classmethod
 def g(cls):
  cls.m()
