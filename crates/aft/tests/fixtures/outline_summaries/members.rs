pub struct Service;
impl Service {
    fn private_first(&self) {}
    pub fn first(&self) {}
    pub fn second(&self) {}
    fn private_second(&self) {}
}
impl Service {
    pub fn third() -> Self { Self }
    pub fn fourth(&self) {}
}
pub enum Small { A, B }
impl Small {
    pub fn only(&self) {}
}
