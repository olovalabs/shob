# Shob shell integration for bash.
#
# Sourced from a generated --rcfile after the user's ~/.bashrc. Emits OSC 133
# semantic prompt markers carrying Shob's per-run token, plus the cwd via
# OSC 7 and metadata via OSC 7502.

[[ $- == *i* ]] || return 0
[[ -n $SHOB_SHELL_INTEGRATION ]] && return 0
export SHOB_SHELL_INTEGRATION=1

# Keep the token in a shell variable and remove it from the environment so
# child processes (agents, TUIs) cannot stamp valid markers.
__SHOB_TOKEN=$SHOB_SHELL_TOKEN
unset SHOB_SHELL_TOKEN

__shob_osc() {
  printf '\033]%s\007' "$1"
}

# Runs first in PROMPT_COMMAND, before any user prompt hook, so it can see the
# real exit status. The in-prompt flag suppresses C while prompt hooks run.
__shob_prompt_start() {
  __SHOB_LAST_STATUS=$?
  __SHOB_IN_PROMPT=1
}

# Runs last in PROMPT_COMMAND, immediately before the prompt is drawn.
__shob_precmd() {
  local ret=$__SHOB_LAST_STATUS
  __SHOB_IN_COMMAND=
  __SHOB_IN_PROMPT=
  __SHOB_LAST_STATUS=
  __shob_osc "133;D;${ret};SHOB=${__SHOB_TOKEN}"
  __shob_osc "7;file://${HOSTNAME}${PWD}"
  __shob_osc "7502;host=${HOSTNAME}"
  return $ret
}

# The DEBUG trap is bash's closest equivalent of zsh's preexec.
__shob_preexec() {
  [[ -n $__SHOB_IN_PROMPT ]] && return
  [[ $BASH_COMMAND == __shob_* ]] && return
  [[ -n $__SHOB_IN_COMMAND ]] && return
  __SHOB_IN_COMMAND=1
  __shob_osc "133;C;SHOB=${__SHOB_TOKEN}"
}

# A starts the prompt, B ends it. \[...\] keeps both zero-width.
PS1="\[\e]133;A;SHOB=${__SHOB_TOKEN}\a\]${PS1}\[\e]133;B;SHOB=${__SHOB_TOKEN}\a\]"

# Bracket the user's prompt commands so their DEBUG traps do not look like a
# command being executed.
if [[ ";${PROMPT_COMMAND};" != *";__shob_prompt_start;"* ]]; then
  PROMPT_COMMAND="__shob_prompt_start;${PROMPT_COMMAND:+${PROMPT_COMMAND};}__shob_precmd"
fi

# An existing DEBUG trap is replaced; users who need theirs should disable
# integration with SHOB_NO_SHELL_INTEGRATION=1.
trap '__shob_preexec' DEBUG
