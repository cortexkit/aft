namespace api {
class Widget {
public:
    int value() { return 42; }
};
}
inline int use_widget() { api::Widget widget; return widget.value(); }
