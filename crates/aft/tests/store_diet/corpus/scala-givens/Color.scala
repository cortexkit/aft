package demo

enum Color {
  case Red, Green, Blue
  def describe: String = this match { case Red => "r"; case Green => "g"; case Blue => "b" }
}

given Ordering[Int] = Ordering.fromLessThan(_ < _)

given intShow: Show[Int] = Show.show(_.toString)

object Module {
  given stringEq: Eq[String] = (a, b) => a == b
}
