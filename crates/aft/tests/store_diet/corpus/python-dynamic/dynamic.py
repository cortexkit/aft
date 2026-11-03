class A:
 def m(self): pass
class B:
 def m(self): pass
 def n(self): pass
def caller(x, name):
 x.m()
 getattr(x, name)()
