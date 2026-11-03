
namespace Foo { void bar(); }
struct Painter { void draw(); };
void foo();
void caller(Painter *p, Painter obj) {
    foo();
    obj.draw();
    p->draw();
    Foo::bar();
    Foo::templ<int>();
}
