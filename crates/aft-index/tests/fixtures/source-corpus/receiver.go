package p
type C struct{}
func (c C) m() {}
func (c C) f(x C) { c.m(); x.m(); var y C; y.m(); z := C{}; z.m(); w := &C{}; w.m() }