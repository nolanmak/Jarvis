import OpenAI from "openai";
import dotenv from "dotenv";

dotenv.config();

const client = new OpenAI({
  apiKey: process.env.GROQ_API_KEY,
  baseURL: "https://api.groq.com/openai/v1",
});

const SYSTEM_PROMPT = `You are an AI email assistant. Draft professional, concise replies to emails.
- Match the tone of the original email (formal/casual).
- Keep replies brief and to the point.
- If the email requires action, acknowledge it clearly.
- Sign off appropriately.`;

export async function generateReply(emailThread: string): Promise<string> {
  const response = await client.chat.completions.create({
    model: "llama-3.3-70b-versatile",
    messages: [
      { role: "system", content: SYSTEM_PROMPT },
      {
        role: "user",
        content: `Draft a reply to this email thread:\n\n${emailThread}`,
      },
    ],
    temperature: 0.7,
    max_tokens: 1024,
  });

  const reply = response.choices[0]?.message?.content;
  if (!reply) {
    throw new Error("LLM returned empty response");
  }
  return reply;
}
