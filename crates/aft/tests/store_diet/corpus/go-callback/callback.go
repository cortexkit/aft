package main
func callback() {}
func entry() { spawn(callback); saved := callback; _ = saved }
