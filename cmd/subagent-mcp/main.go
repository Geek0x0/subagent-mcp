package main

import (
	"flag"
	"fmt"
	"log"
	"os"

	"github.com/Geek0x0/subagent-mcp/internal/config"
	_ "github.com/Geek0x0/subagent-mcp/internal/provider/chatcompletions"
	_ "github.com/Geek0x0/subagent-mcp/internal/provider/codexappserver"
	_ "github.com/Geek0x0/subagent-mcp/internal/provider/messages"
	_ "github.com/Geek0x0/subagent-mcp/internal/provider/responses"
	"github.com/Geek0x0/subagent-mcp/internal/sandbox"
	dsserver "github.com/Geek0x0/subagent-mcp/internal/server"
	"github.com/Geek0x0/subagent-mcp/internal/tools"
)

const version = "0.10.1"

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
		os.Exit(runCheck(os.Stdout, cfg, *liveFlag))
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

	if err := dsserver.New(cfg, version, options...).ServeStdio(); err != nil {
		log.Fatal(err)
	}
}

func configureRuntime(cfg *config.Config) {
	if err := disableProcessDumpingFn(); err != nil {
		fmt.Fprintf(os.Stderr, "warning: /proc environment protection could not be enabled: %v; continuing\n", err)
	}
	tools.SetScrubbedEnv(cfg.EnvKeys(), []string{"SUBAGENT_MCP_"})
	tools.SetProtectedFiles([]string{cfg.Path})
}
