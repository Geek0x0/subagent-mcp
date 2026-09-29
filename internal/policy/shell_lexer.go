package policy

import "strings"

type shellSegment struct {
	words []string
}

// lexShell splits a command into the simple-command segments that the shell
// will execute. It performs the part of shell lexing that is useful to the
// allowlist: quote removal, backslash handling, and command separators. It
// deliberately rejects expansions and redirections rather than attempting to
// model their runtime effects.
func lexShell(command string) ([]shellSegment, bool) {
	var segments []shellSegment
	var segment []string
	var word strings.Builder
	wordStarted := false
	tildeEligible := true
	quote := byte(0)
	comment := false
	needCommand := false

	flushWord := func() {
		if !wordStarted {
			return
		}
		segment = append(segment, word.String())
		word.Reset()
		wordStarted = false
		tildeEligible = true
	}
	flushSegment := func() {
		flushWord()
		if len(segment) == 0 {
			return
		}
		segments = append(segments, shellSegment{words: segment})
		segment = nil
	}

	for i := 0; i < len(command); {
		c := command[i]

		if comment {
			if c == '\n' {
				comment = false
				if needCommand {
					return nil, false
				}
				flushSegment()
			}
			i++
			continue
		}

		switch quote {
		case '\'':
			if c == '\'' {
				quote = 0
			} else {
				word.WriteByte(c)
			}
			i++
			continue
		case '"':
			switch c {
			case '"':
				quote = 0
				i++
			case '\\':
				if i+1 >= len(command) {
					return nil, false
				}
				next := command[i+1]
				if next == '\n' {
					i += 2
					continue
				}
				if next == '$' || next == '`' || next == '"' || next == '\\' {
					word.WriteByte(next)
					i += 2
					continue
				}
				word.WriteByte('\\')
				i++
			case '$', '`':
				// Both parameter expansion and command substitution are
				// active inside double quotes.
				return nil, false
			default:
				word.WriteByte(c)
				i++
			}
			continue
		}

		switch c {
		case '\\':
			if i+1 >= len(command) {
				return nil, false
			}
			needCommand = false
			next := command[i+1]
			if next == '\n' {
				i += 2
				continue
			}
			word.WriteByte(next)
			wordStarted = true
			tildeEligible = false
			i += 2
		case '\'':
			quote = '\''
			wordStarted = true
			tildeEligible = false
			needCommand = false
			i++
		case '"':
			quote = '"'
			wordStarted = true
			tildeEligible = false
			needCommand = false
			i++
		case ' ', '\t':
			flushWord()
			i++
		case '\n':
			flushSegment()
			if needCommand {
				return nil, false
			}
			needCommand = false
			i++
		case ';':
			if needCommand {
				return nil, false
			}
			flushSegment()
			if len(segments) == 0 {
				return nil, false
			}
			needCommand = false
			i++
		case '&':
			if needCommand {
				return nil, false
			}
			flushWord()
			binary := i+1 < len(command) && command[i+1] == '&'
			if binary && len(segment) == 0 {
				return nil, false
			}
			flushSegment()
			if len(segments) == 0 {
				return nil, false
			}
			needCommand = binary
			if binary {
				i += 2
			} else {
				i++
			}
		case '|':
			flushWord()
			binary := i+1 < len(command) && command[i+1] == '|'
			if len(segment) == 0 {
				return nil, false
			}
			flushSegment()
			needCommand = true
			if binary {
				i += 2
			} else {
				i++
			}
		case '<', '>':
			// This also rejects heredocs and process substitution. Quoted
			// and escaped forms were handled above and are ordinary data.
			return nil, false
		case '$', '`':
			// Expansion can execute code or change the argv seen by the
			// command, so it is never safe for an auto-allowed command.
			return nil, false
		case '*', '?', '[', '{', '}', '(', ')':
			// Pathname, brace, and tilde expansion can turn a filename into
			// an option. Parentheses are shell syntax rather than a word.
			return nil, false
		case '~':
			// Bash performs tilde expansion only at the beginning of a word
			// and after an unquoted equals sign. A revision such as HEAD~1
			// is literal data.
			if tildeEligible {
				return nil, false
			}
			word.WriteByte(c)
			wordStarted = true
			tildeEligible = false
			i++
		case '#':
			if !wordStarted {
				comment = true
				i++
				continue
			}
			word.WriteByte(c)
			wordStarted = true
			tildeEligible = false
			needCommand = false
			i++
		default:
			word.WriteByte(c)
			wordStarted = true
			tildeEligible = c == '='
			needCommand = false
			i++
		}
	}

	if quote != 0 || needCommand {
		return nil, false
	}
	flushSegment()
	return segments, len(segments) > 0
}
