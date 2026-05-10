import { BusEvent } from "@/bus/bus-event"
import { InstanceState } from "@/effect/instance-state"
import type { InstanceContext } from "@/project/instance"
import { SessionID, MessageID } from "@/session/schema"
import { Effect, Layer, Context } from "effect"
import { EffectLogger } from "@/effect/logger"
import z from "zod"
import { Config } from "../config/config"
import { MCP } from "../mcp"
import { Skill } from "../skill"
import { Log } from "../util/log"
import PROMPT_INITIALIZE from "./template/initialize.txt"
import PROMPT_REVIEW from "./template/review.txt"

export namespace Command {
const log = Log.create({ service: "command" })

const PROMPT_CREATE_SKILLS = [
  "Create new OpenCode skill from user request.",
  "",
  "Request:",
  "$ARGUMENTS",
  "",
  "Requirements:",
  "- Create exactly one file at skills/<slug>/SKILL.md.",
  "- Use lowercase kebab-case slug from request topic.",
  "- File must start with yaml frontmatter:",
  "  ---",
  "  name: <Skill Name>",
  "  description: <one-line description>",
  "  ---",
  "- Then write practical skill guide with clear workflow steps.",
  "- Keep content concise, actionable, production-ready.",
  "- Prefer ASCII.",
  "",
  "Validation:",
  "- Ensure file path exists and content matches format above.",
  "- At end, print created file path and one-line usage note.",
].join("\n")

const PROMPT_REMOVE_SKILL = [
  "Remove OpenCode skill by name.",
  "",
  "Input skill:",
  "$ARGUMENTS",
  "",
  "Requirements:",
  "- Find matching SKILL.md for given skill name.",
  "- Delete skill folder safely from skills/<slug>/ or .opencode skill paths in workspace.",
  "- Do not touch unrelated files.",
  "- If not found, explain clearly and stop.",
  "",
  "Validation:",
  "- Confirm deleted path.",
  "- Confirm skill no longer appears in list after refresh.",
].join("\n")

  type State = {
    commands: Record<string, Info>
  }

  export const Event = {
    Executed: BusEvent.define(
      "command.executed",
      z.object({
        name: z.string(),
        sessionID: SessionID.zod,
        arguments: z.string(),
        messageID: MessageID.zod,
      }),
    ),
  }

  export const Info = z
    .object({
      name: z.string(),
      description: z.string().optional(),
      agent: z.string().optional(),
      model: z.string().optional(),
      source: z.enum(["command", "mcp", "skill"]).optional(),
      // workaround for zod not supporting async functions natively so we use getters
      // https://zod.dev/v4/changelog?id=zfunction
      template: z.promise(z.string()).or(z.string()),
      subtask: z.boolean().optional(),
      hints: z.array(z.string()),
    })
    .meta({
      ref: "Command",
    })

  // for some reason zod is inferring `string` for z.promise(z.string()).or(z.string()) so we have to manually override it
  export type Info = Omit<z.infer<typeof Info>, "template"> & { template: Promise<string> | string }

  export function hints(template: string) {
    const result: string[] = []
    const numbered = template.match(/\$\d+/g)
    if (numbered) {
      for (const match of [...new Set(numbered)].sort()) result.push(match)
    }
    if (template.includes("$ARGUMENTS")) result.push("$ARGUMENTS")
    return result
  }

  export const Default = {
    INIT: "init",
    REVIEW: "review",
    CREATE_SKILLS: "create-skills",
    REMOVE_SKILL: "remove-skill",
  } as const

  export interface Interface {
    readonly get: (name: string) => Effect.Effect<Info | undefined>
    readonly list: () => Effect.Effect<Info[]>
  }

  export class Service extends Context.Service<Service, Interface>()("@opencode/Command") {}

  export const layer = Layer.effect(
    Service,
    Effect.gen(function* () {
      const config = yield* Config.Service
      const mcp = yield* MCP.Service
      const skill = yield* Skill.Service

      const init = Effect.fn("Command.state")(function* (ctx: InstanceContext) {
        const cfg = yield* config.get()
        const commands: Record<string, Info> = {}

        commands[Default.INIT] = {
          name: Default.INIT,
          description: "guided AGENTS.md setup",
          source: "command",
          get template() {
            return PROMPT_INITIALIZE.replace("${path}", ctx.worktree)
          },
          hints: hints(PROMPT_INITIALIZE),
        }
        commands[Default.REVIEW] = {
          name: Default.REVIEW,
          description: "review changes [commit|branch|pr], defaults to uncommitted",
          source: "command",
          get template() {
            return PROMPT_REVIEW.replace("${path}", ctx.worktree)
          },
          subtask: true,
          hints: hints(PROMPT_REVIEW),
        }
        commands[Default.CREATE_SKILLS] = {
          name: Default.CREATE_SKILLS,
          description: "create custom skill from topic",
          source: "command",
          get template() {
            return PROMPT_CREATE_SKILLS
          },
          hints: hints(PROMPT_CREATE_SKILLS),
        }
        commands[Default.REMOVE_SKILL] = {
          name: Default.REMOVE_SKILL,
          description: "remove skill by name",
          source: "command",
          get template() {
            return PROMPT_REMOVE_SKILL
          },
          hints: hints(PROMPT_REMOVE_SKILL),
        }

        for (const [name, command] of Object.entries(cfg.command ?? {})) {
          commands[name] = {
            name,
            agent: command.agent,
            model: command.model,
            description: command.description,
            source: "command",
            get template() {
              return command.template
            },
            subtask: command.subtask,
            hints: hints(command.template),
          }
        }

        for (const [name, prompt] of Object.entries(yield* mcp.prompts())) {
          commands[name] = {
            name,
            source: "mcp",
            description: prompt.description,
            get template() {
              return Effect.runPromise(
                mcp
                  .getPrompt(
                    prompt.client,
                    prompt.name,
                    prompt.arguments
                      ? Object.fromEntries(prompt.arguments.map((argument, i) => [argument.name, `$${i + 1}`]))
                      : {},
                  )
                  .pipe(
                    Effect.map(
                      (template) =>
                        template?.messages
                          .map((message) => (message.content.type === "text" ? message.content.text : ""))
                          .join("\n") || "",
                    ),
                    Effect.provide(EffectLogger.layer),
                  ),
              )
            },
            hints: prompt.arguments?.map((_, i) => `$${i + 1}`) ?? [],
          }
        }

        for (const item of yield* skill.all()) {
          if (commands[item.name]) continue
          commands[item.name] = {
            name: item.name,
            description: item.description,
            source: "skill",
            get template() {
              return item.content
            },
            hints: [],
          }
        }

        return {
          commands,
        }
      })

      const state = yield* InstanceState.make<State>((ctx) => init(ctx))

      const get = Effect.fn("Command.get")(function* (name: string) {
        const s = yield* InstanceState.get(state)
        return s.commands[name]
      })

      const list = Effect.fn("Command.list")(function* () {
        const s = yield* InstanceState.get(state)
        return Object.values(s.commands)
      })

      return Service.of({ get, list })
    }),
  )

  export const defaultLayer = layer.pipe(
    Layer.provide(Config.defaultLayer),
    Layer.provide(MCP.defaultLayer),
    Layer.provide(Skill.defaultLayer),
  )
}
