pub struct Widget {
    pub name: String,
    pub size: u32,
}

pub fn build_widget(name: &str, size: u32) -> Widget {
    Widget {
        name: name.to_string(),
        size,
    }
}

pub fn describe(w: &Widget) -> String {
    format!("{}:{}", w.name, w.size)
}
