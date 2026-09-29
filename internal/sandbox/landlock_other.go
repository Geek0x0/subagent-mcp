//go:build !linux

package sandbox

import "errors"

var errUnsupported = errors.New("landlock unavailable: kernel sandbox requires Linux")

// Available reports whether the kernel supports Landlock.
func Available() error { return errUnsupported }

func run(args []string) error {
	if _, err := parseArgs(args); err != nil {
		return err
	}
	return errUnsupported
}
