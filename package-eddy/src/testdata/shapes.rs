pub struct Circle {
    pub radius: f64,
}

impl Circle {
    pub fn area(&self) -> f64 {
        3.14 * self.radius * self.radius
    }
}

pub fn total(shapes: &[Circle]) -> f64 {
    shapes.iter().map(|c| c.area()).sum()
}
