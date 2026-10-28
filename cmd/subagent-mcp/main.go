package main

import (
	"context"
	"errors"
	"flag"
	"fmt"
	"log"
	"os"
	"os/signal"
	"syscall"
	"time"

	"github.com/Geek0x0/subagent-mcp/internal/config"
	"github.com/Geek0x0/subagent-mcp/internal/provider"
	_ "github.com/Geek0x0/subagent-mcp/internal/provider/chatcompletions"
	_ "github.com/Geek0x0/subagent-mcp/internal/provider/codexappserver"
	_ "github.com/Geek0x0/subagent-mcp/internal/provider/messages"
	_ "github.com/Geek0x0/subagent-mcp/internal/provider/responses"
	"github.com/Geek0x0/subagent-mcp/internal/sandbox"
	dsserver "github.com/Geek0x0/subagent-mcp/internal/server"
	"github.com/Geek0x0/subagent-mcp/internal/tools"
)

const version = "0.11.2"

// shutdownGrace bounds how long the process waits for in-flight runs to unwind
// and for sessions to be released. Cancellation reaches every blocking step a
// run makes, so this only elapses when a dependency ignores its context.
const shutdownGrace = 10 * time.Second

var disableProcessDumpingFn = disableProcessDumping

func main() {
	sandbox.MaybeRunHelper()
	showVersion := flag.Bool("version", false, "print version and exit")
	checkConfig := flag.Bool("check-config", false, "validate the config file and each provider's reachability, then exit")
	liveFlag := flag.Bool("live", false, "with --check-config, make a real minimal call to each reachable provider (billable)")
	flag.Parse()
	if *showVersion {
		fmt.Printf("subagent-mcp %s\n", version)
		return
	}
	if *checkConfig {
		path := flag.Arg(0)
		if path == "" {
			defaultPath, err := config.DefaultPath()
			if err != nil {
				fmt.Printf("config   FAIL: %v\n", err)
				os.Exit(1)
			}
			path = defaultPath
		}
		cfg, err := config.Load(path)
		if err != nil {
			fmt.Printf("config   %s   FAIL: %v\n", path, err)
			os.Exit(1)
		}
		configureRuntime(cfg)
		fmt.Printf("config   %s   OK\n", path)
		reportSandbox(os.Stdout)
		code := runCheck(os.Stdout, cfg, *liveFlag)
		// Checking a codex-app-server provider starts its child, so release it
		// before exiting rather than leaving it to stdin EOF.
		provider.Shutdown()
		os.Exit(code)
	}

	path, err := config.DefaultPath()
	if err != nil {
		log.Fatal(err)
	}
	cfg, err := config.Load(path)
	if err != nil {
		log.Fatal(err)
	}
	configureRuntime(cfg)

	var options []dsserver.Option
	if toolName := os.Getenv("SUBAGENT_MCP_TOOL_NAME"); toolName != "" {
		if err := dsserver.ValidateToolName(toolName); err != nil {
			log.Fatal(err)
		}
		options = append(options, dsserver.WithToolName(toolName))
	}

	os.Exit(serve(dsserver.New(cfg, version, options...)))
}

// serve runs the stdio loop until the client closes its end or a signal asks the
// server to stop, then shuts down within shutdownGrace and reports the exit
// code. Provider resources are released last: pooled child processes must
// outlive the sessions that use them.
func serve(srv *dsserver.Server) int {
	signals := make(chan os.Signal, 2)
	signal.Notify(signals, syscall.SIGINT, syscall.SIGTERM)
	defer signal.Stop(signals)

	served := make(chan error, 1)
	go func() { served <- srv.ServeStdio() }()

	serving := true
	var serveErr error
	select {
	case serveErr = <-served:
		serving = false
	case sig := <-signals:
		log.Printf("received %s; shutting down", sig)
		// A second signal abandons the wait: an operator must always be able to
		// stop a server parked on a dependency that ignores cancellation.
		go func() {
			<-signals
			log.Printf("received a second signal; exiting immediately")
			os.Exit(130)
		}()
	}

	ctx, cancel := context.WithTimeout(context.Background(), shutdownGrace)
	defer cancel()
	srv.Close(ctx)

	if serving {
		select {
		case serveErr = <-served:
		case <-ctx.Done():
			log.Printf("shutdown: the stdio loop did not stop within %s", shutdownGrace)
		}
	}
	// Provider resources go last, and within the same budget: a pool whose child
	// is being dialed holds its gate until that dial returns, and shutdown must
	// not be able to outlast the grace period it promises.
	released := make(chan struct{})
	go func() {
		defer close(released)
		provider.Shutdown()
	}()
	select {
	case <-released:
	case <-ctx.Done():
		log.Printf("shutdown: a provider did not release its resources within %s", shutdownGrace)
	}

	// Ending the serve loop is how Close stops it, so a shutdown this process
	// asked for reports a cancellation rather than an error; a client that
	// simply closes its end reports nil. Both mean the server stopped on
	// purpose.
	if serveErr != nil && !errors.Is(serveErr, context.Canceled) {
		log.Print(serveErr)
		return 1
	}
	return 0
}

func configureRuntime(cfg *config.Config) {
	if err := disableProcessDumpingFn(); err != nil {
		fmt.Fprintf(os.Stderr, "warning: /proc environment protection could not be enabled: %v; continuing\n", err)
	}
	tools.SetScrubbedEnv(cfg.EnvKeys(), []string{"SUBAGENT_MCP_"})
	tools.SetProtectedFiles([]string{cfg.Path})
}
