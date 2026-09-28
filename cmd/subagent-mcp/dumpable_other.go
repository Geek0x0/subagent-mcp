//go:build !linux

package main

func disableProcessDumping() error { return nil }
