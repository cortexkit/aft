
int foo(void);
struct Obj { int (*method)(void); };
void caller(struct Obj *p, struct Obj obj) {
    foo();
    obj.method();
    p->method();
}
