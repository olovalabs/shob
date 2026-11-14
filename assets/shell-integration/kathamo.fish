# Kathamo shell integration for fish.
#
# Loaded with `fish --init-command`. Emits OSC 133 semantic prompt markers
# carrying Kathamo's per-run token, the cwd via OSC 7, and metadata via OSC
# 7502. fish 4.x also emits its own 133 markers; the token lets the terminal
# ignore those and only trust ours.

if status is-interactive
    if not set -q KATHAMO_SHELL_INTEGRATION
        set -gx KATHAMO_SHELL_INTEGRATION 1

        # Keep the token in a shell variable and remove it from the environment
        # so child processes (agents, TUIs) cannot stamp valid markers.
        set -g __kathamo_token $KATHAMO_SHELL_TOKEN
        set -e KATHAMO_SHELL_TOKEN

        function __kathamo_osc
            printf '\e]%s\a' $argv[1]
        end

        # Preserve the user's prompt function, then wrap it with A/B markers.
        if functions -q fish_prompt
            functions -c fish_prompt __kathamo_orig_prompt
        end

        function fish_prompt
            __kathamo_osc "133;A;KATHAMO=$__kathamo_token"
            if functions -q __kathamo_orig_prompt
                __kathamo_orig_prompt
            else
                printf '%s@%s %s $ ' (whoami) (hostname -s 2>/dev/null; or uname -n) (prompt_pwd)
            end
            __kathamo_osc "133;B;KATHAMO=$__kathamo_token"
        end

        function __kathamo_preexec --on-event fish_preexec
            __kathamo_osc "133;C;KATHAMO=$__kathamo_token"
        end

        function __kathamo_postexec --on-event fish_postexec
            set -l ret $status
            __kathamo_osc "133;D;$ret;KATHAMO=$__kathamo_token"
            __kathamo_osc "7;file://"(hostname)(pwd)
            __kathamo_osc "7502;host="(hostname)
        end
    end
end
