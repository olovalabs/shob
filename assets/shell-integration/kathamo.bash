# Kathamo shell integration for bash.
#
# Sourced from a generated --rcfile after the user's ~/.bashrc. Emits OSC 133
# semantic prompt markers carrying Kathamo's per-run token, plus the cwd via
# OSC 7 and metadata via OSC 7502.

[[ $- == *i* ]] || return 0
[[ -n $KATHAMO_SHELL_INTEGRATION ]] && return 0
export KATHAMO_SHELL_INTEGRATION=1

# Keep the token in a shell variable and remove it from the environment so
# child processes (agents, TUIs) cannot stamp valid markers.
__KATHAMO_TOKEN=$KATHAMO_SHELL_TOKEN
unset KATHAMO_SHELL_TOKEN

__kathamo_osc() {
  printf '\033]%s\007' "$1"
}

# Runs first in PROMPT_COMMAND, before any user prompt hook, so it can see the
# real exit status. The in-prompt flag suppresses C while prompt hooks run.
__kathamo_prompt_start() {
  __KATHAMO_LAST_STATUS=$?
  __KATHAMO_IN_PROMPT=1
}

# Runs last in PROMPT_COMMAND, immediately before the prompt is drawn.
__kathamo_precmd() {
  local ret=$__KATHAMO_LAST_STATUS
  __KATHAMO_IN_COMMAND=
  __KATHAMO_IN_PROMPT=
  __KATHAMO_LAST_STATUS=
  __kathamo_osc "133;D;${ret};KATHAMO=${__KATHAMO_TOKEN}"
  __kathamo_osc "7;file://${HOSTNAME}${PWD}"
  __kathamo_osc "7502;host=${HOSTNAME}"
  return $ret
}

# The DEBUG trap is bash's closest equivalent of zsh's preexec.
__kathamo_preexec() {
  [[ -n $__KATHAMO_IN_PROMPT ]] && return
  [[ $BASH_COMMAND == __kathamo_* ]] && return
  [[ -n $__KATHAMO_IN_COMMAND ]] && return
  __KATHAMO_IN_COMMAND=1
  __kathamo_osc "133;C;KATHAMO=${__KATHAMO_TOKEN}"
}

# A starts the prompt, B ends it. \[...\] keeps both zero-width.
PS1="\[\e]133;A;KATHAMO=${__KATHAMO_TOKEN}\a\]${PS1}\[\e]133;B;KATHAMO=${__KATHAMO_TOKEN}\a\]"

# Bracket the user's prompt commands so their DEBUG traps do not look like a
# command being executed.
if [[ ";${PROMPT_COMMAND};" != *";__kathamo_prompt_start;"* ]]; then
  PROMPT_COMMAND="__kathamo_prompt_start;${PROMPT_COMMAND:+${PROMPT_COMMAND};}__kathamo_precmd"
fi

# An existing DEBUG trap is replaced; users who need theirs should disable
# integration with KATHAMO_NO_SHELL_INTEGRATION=1.
trap '__kathamo_preexec' DEBUG
