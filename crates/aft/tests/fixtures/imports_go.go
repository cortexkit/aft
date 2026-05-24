// Test fixture: Go file with stdlib and external import groups
// Used by integration tests for add_import command
// Includes both single and grouped import forms

package main

import "fmt"

import (
	"os"
	"strings"

	"github.com/pkg/errors"
	"github.com/sirupsen/logrus"
)

func main() {
	fmt.Println("hello")
	_ = os.Getenv("PATH")
	_ = strings.Contains("a", "b")
	_ = errors.New("err")
	logrus.Info("starting")
}
