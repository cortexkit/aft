package p
type I interface { m() }
type A struct{}
func (a A) m() {}
type B struct{}
func (b B) m() {}
func caller(x I) { x.m() }