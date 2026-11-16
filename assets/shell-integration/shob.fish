# Shob shell integration for fish.
#
# Loaded with `fish --init-command`. Emits OSC 133 semantic prompt markers
# carrying Shob's per-run token, the cwd via OSC 7, and metadata via OSC
# 7502. fish 4.x also emits its own 133 markers; the token lets the terminal
# ignore those and only trust ours.

if status is-interactive
    if not set -q SHOB_SHELL_INTEGRATION
        set -gx SHOB_SHELL_INTEGRATION 1

        # Keep the token in a shell variable and remove it from the environment
        # so child processes (agents, TUIs) cannot stamp valid markers.
        set -g __shob_token $SHOB_SHELL_TOKEN
        set -e SHOB_SHELL_TOKEN

        function __shob_osc
            printf '\e]%s\a' $argv[1]
        end

        # Preserve the user's prompt function, then wrap it with A/B markers.
        if functions -q fish_prompt
            functions -c fish_prompt __shob_orig_prompt
        end

        function fish_prompt
            __shob_osc "133;A;SHOB=$__shob_token"
            if functions -q __shob_orig_prompt
                __shob_orig_prompt
            else
                printf '%s@%s %s $ ' (whoami) (hostname -s 2>/dev/null; or uname -n) (prompt_pwd)
            end
            __shob_osc "133;B;SHOB=$__shob_token"
        end

        function __shob_preexec --on-event fish_preexec
            __shob_osc "133;C;SHOB=$__shob_token"
        end

        function __shob_postexec --on-event fish_postexec
            set -l ret $status
            __shob_osc "133;D;$ret;SHOB=$__shob_token"
            __shob_osc "7;file://"(hostname)(pwd)
            __shob_osc "7502;host="(hostname)
        end
    end
end
