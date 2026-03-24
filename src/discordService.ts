import {
  Client,
  GatewayIntentBits,
  ActionRowBuilder,
  ButtonBuilder,
  ButtonStyle,
  TextChannel,
  ComponentType,
  ModalBuilder,
  TextInputBuilder,
  TextInputStyle,
} from "discord.js";
import type { ThreadChannel, ButtonInteraction } from "discord.js";
import type { Email } from "./types";
import dotenv from "dotenv";

dotenv.config();

// Lazy import to avoid circular dependency — set by index.ts after agent is ready
let _redraftFn: ((original: Email, previousDraft: string, feedback: string) => Promise<string>) | null = null;

export function setRedraftFunction(fn: typeof _redraftFn): void {
  _redraftFn = fn;
}

const client = new Client({
  intents: [
    GatewayIntentBits.Guilds,
    GatewayIntentBits.GuildMessages,
    GatewayIntentBits.MessageContent,
  ],
});

let isReady = false;

export async function initBot(): Promise<void> {
  return new Promise((resolve, reject) => {
    client.once("ready", () => {
      console.log(`Discord bot logged in as ${client.user?.tag}`);
      isReady = true;
      resolve();
    });

    client.login(process.env.DISCORD_BOT_TOKEN).catch(reject);
  });
}

function formatEmailSummary(original: Email): string {
  const bodyPreview = original.body.length > 500
    ? original.body.slice(0, 500) + "..."
    : original.body;

  return [
    `**From:** ${original.from}`,
    `**Subject:** ${original.subject}`,
    `**Date:** ${original.date}`,
    "",
    "**Original email:**",
    `> ${bodyPreview.split("\n").join("\n> ")}`,
  ].join("\n");
}

function formatDraftMessage(draft: string, revision?: number): string {
  const header = revision
    ? `**Draft (revision ${revision}):**`
    : "**Draft reply:**";
  return `${header}\n\n${draft}`;
}

function buildButtons(): ActionRowBuilder<ButtonBuilder> {
  return new ActionRowBuilder<ButtonBuilder>().addComponents(
    new ButtonBuilder()
      .setCustomId("approve")
      .setLabel("Approve & Send")
      .setStyle(ButtonStyle.Success),
    new ButtonBuilder()
      .setCustomId("revise")
      .setLabel("Revise")
      .setStyle(ButtonStyle.Secondary),
    new ButtonBuilder()
      .setCustomId("reject")
      .setLabel("Skip")
      .setStyle(ButtonStyle.Danger),
  );
}

export interface ApprovalResult {
  approved: boolean;
  finalDraft: string;
}

export async function sendForApproval(
  original: Email,
  draft: string
): Promise<ApprovalResult> {
  if (!isReady) {
    throw new Error("Discord bot is not initialized. Call initBot() first.");
  }

  const channelId = process.env.DISCORD_CHANNEL_ID;
  if (!channelId) {
    throw new Error("DISCORD_CHANNEL_ID is not set");
  }

  const channel = await client.channels.fetch(channelId);
  if (!channel || !(channel instanceof TextChannel)) {
    throw new Error(`Channel ${channelId} not found or is not a text channel`);
  }

  // Create a thread for this email conversation
  const threadName = `${original.from} - ${original.subject}`.slice(0, 100);
  const thread = await channel.threads.create({
    name: threadName,
    autoArchiveDuration: 60,
  });

  // Post email summary in thread
  await thread.send({ content: formatEmailSummary(original) });

  // Post initial draft with buttons
  let currentDraft = draft;
  let revision = 0;

  const draftMessage = await thread.send({
    content: formatDraftMessage(currentDraft),
    components: [buildButtons()],
  });

  // Approval loop — supports multiple revise rounds
  return new Promise<ApprovalResult>((resolve) => {
    const TIMEOUT_MS = 60 * 60 * 1000; // 1 hour
    let settled = false;

    function settle(result: ApprovalResult): void {
      if (settled) return;
      settled = true;
      resolve(result);
    }

    // Set up a timeout
    const timeout = setTimeout(async () => {
      if (settled) return;
      await thread.send({ content: "**Timed out** -- no action taken. Thread will be archived." });
      settle({ approved: false, finalDraft: currentDraft });
    }, TIMEOUT_MS);

    // Listen for button interactions in this thread
    const collector = thread.createMessageComponentCollector({
      componentType: ComponentType.Button,
      time: TIMEOUT_MS,
    });

    collector.on("collect", async (interaction: ButtonInteraction) => {
      if (settled) return;

      switch (interaction.customId) {
        case "approve": {
          await interaction.update({
            content: formatDraftMessage(currentDraft, revision || undefined) + "\n\n**APPROVED** -- sending email.",
            components: [],
          });
          clearTimeout(timeout);
          collector.stop();
          settle({ approved: true, finalDraft: currentDraft });
          break;
        }

        case "reject": {
          await interaction.update({
            content: formatDraftMessage(currentDraft, revision || undefined) + "\n\n**SKIPPED** -- no email sent.",
            components: [],
          });
          clearTimeout(timeout);
          collector.stop();
          settle({ approved: false, finalDraft: currentDraft });
          break;
        }

        case "revise": {
          // Show modal for feedback
          const modalId = `revise_modal_${Date.now()}`;
          const modal = new ModalBuilder()
            .setCustomId(modalId)
            .setTitle("Revise Draft");

          const feedbackInput = new TextInputBuilder()
            .setCustomId("feedback")
            .setLabel("What should change?")
            .setStyle(TextInputStyle.Paragraph)
            .setPlaceholder("e.g. Make it shorter, more formal, ask about timeline...")
            .setRequired(true);

          const row = new ActionRowBuilder<TextInputBuilder>().addComponents(feedbackInput);
          modal.addComponents(row);

          await interaction.showModal(modal);

          try {
            // Wait for modal submission (5 min timeout)
            const modalSubmit = await interaction.awaitModalSubmit({
              filter: (i) => i.customId === modalId,
              time: 5 * 60 * 1000,
            });

            const feedback = modalSubmit.fields.getTextInputValue("feedback");
            await modalSubmit.deferUpdate();

            // Show "revising..." message
            await thread.send({ content: `*Revising draft based on feedback: "${feedback}"...*` });

            // Re-draft with feedback
            if (_redraftFn) {
              try {
                revision++;
                currentDraft = await _redraftFn(original, currentDraft, feedback);

                // Post revised draft with new buttons
                await thread.send({
                  content: formatDraftMessage(currentDraft, revision),
                  components: [buildButtons()],
                });
              } catch (err) {
                const msg = err instanceof Error ? err.message : String(err);
                await thread.send({
                  content: `**Revision failed:** ${msg}\n\nOriginal draft still available above.`,
                  components: [buildButtons()],
                });
              }
            } else {
              await thread.send({
                content: "**Revision not available** -- redraft function not configured. Original draft still available above.",
                components: [buildButtons()],
              });
            }
          } catch {
            // Modal timed out — do nothing, buttons still active
            await thread.send({ content: "*Revision cancelled (modal timed out).*" });
          }
          break;
        }
      }
    });

    collector.on("end", () => {
      clearTimeout(timeout);
      if (!settled) {
        settle({ approved: false, finalDraft: currentDraft });
      }
    });
  });
}
