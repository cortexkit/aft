
fn foo() void {}
const Obj = struct {
    fn method(self: *Obj) void {}
};
fn caller(obj: *Obj) void {
    foo();
    obj.method();
    std.debug.print("x", .{});
}
