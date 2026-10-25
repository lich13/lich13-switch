import type { ClientId, GrokProtocol } from "./types";
export const protocolNames: Record<GrokProtocol, string> = {
  responses: "Responses",
  chat_completions: "Chat Completions",
  messages: "Messages",
  gemini_native: "Gemini Native",
};
export const clientProtocols: Record<ClientId, GrokProtocol[]> = {
  codex: ["responses"],
  claude: ["messages"],
  grok: ["responses", "chat_completions", "messages"],
  dsh: ["chat_completions"],
  agy: ["gemini_native"],
};
