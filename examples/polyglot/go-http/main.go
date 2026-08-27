// Command go-http is a plain HTTP server, compiled rather than interpreted.
//
// Every other polyglot example runs its source directly through a language
// runtime shep spawns as `interpreter`; this one shows the other shape a
// Flockfile script takes: a build step first, then a script line with no
// interpreter field at all, because the artifact `go build` produces is
// already a native executable.
//
// Usage: go-http <port>
package main

import (
	"fmt"
	"log"
	"net/http"
	"os"
	"strconv"
)

func main() {
	if len(os.Args) != 2 {
		fmt.Fprintln(os.Stderr, "usage: go-http <port>")
		os.Exit(1)
	}
	port, err := strconv.Atoi(os.Args[1])
	if err != nil {
		fmt.Fprintf(os.Stderr, "go-http: %q is not a valid port: %v\n", os.Args[1], err)
		os.Exit(1)
	}

	http.HandleFunc("/", func(w http.ResponseWriter, r *http.Request) {
		fmt.Fprintf(w, "OK from go pid=%d\n", os.Getpid())
	})

	addr := fmt.Sprintf("127.0.0.1:%d", port)
	log.Printf("go-http pid=%d listening on %s", os.Getpid(), addr)
	if err := http.ListenAndServe(addr, nil); err != nil {
		log.Fatal(err)
	}
}
