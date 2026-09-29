//go:build !linux

package sandbox

import "github.com/Geek0x0/subagent-mcp/internal/patch"

// FS returns nil off Linux: there is no kernel-enforced containment to offer, so
// callers use ordinary paths (and the path-based check in policy).
func (d *Directory) FS() patch.FS { return nil }
