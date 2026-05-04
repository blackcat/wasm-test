// cli-app/main.go
package main

import (
	"fmt"
	"os"
	"strings"

	"example.com/cli-app/gen/example/text/analyze"
)

func main() {
	args := os.Args[1:]
	if len(args) == 0 {
		fmt.Fprintln(os.Stderr, "usage: cli-app <text>")
		os.Exit(1)
	}

	input := strings.Join(args, " ")

	// Call the Rust component through the generated binding.
	// `result` is a result<analysis, analyze-error> lowered into a Go union type.
	result := analyze.AnalyzeText(input, 5)

	if result.IsErr() {
		e := result.Err()
		switch {
		case e.EmptyInput():
			fmt.Fprintln(os.Stderr, "error: empty input")
		case e.TooLarge() != nil:
			fmt.Fprintf(os.Stderr, "error: too large (%d bytes)\n", *e.TooLarge())
		}
		os.Exit(1)
	}

	a := *result.OK()
	fmt.Printf("Word count: %d\n", a.WordCount)
	fmt.Printf("Sentiment:  %.3f\n", a.Sentiment)
	fmt.Printf("Tokens:     %d\n", a.Tokens.Len())

	fmt.Print("Keywords:   ")
	keywords := a.Keywords.Slice()
	for i, k := range keywords {
		if i > 0 {
			fmt.Print(", ")
		}
		fmt.Print(k)
	}
	fmt.Println()
}
