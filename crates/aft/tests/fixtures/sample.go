// Sample Go file for tree-sitter symbol extraction tests.
package sample

import "fmt"

// ExportedFunction is a public function (uppercase).
func ExportedFunction(x int) int {
	return x + 1
}

// unexportedFunction is a private function (lowercase).
func unexportedFunction() {
	fmt.Println("private")
}

// MyStruct is a sample struct type.
type MyStruct struct {
	Name string
	Age  int
}

// Reader is a sample interface type.
type Reader interface {
	Read(p []byte) (n int, err error)
}

// String is a method with receiver on MyStruct.
func (m *MyStruct) String() string {
	return m.Name
}

// helper is an unexported method.
func (m *MyStruct) helper() int {
	return m.Age
}
