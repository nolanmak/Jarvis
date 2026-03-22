import {
  Client,
  GatewayIntentBits,
  ActionRowBuilder,
  ButtonBuilder,
  ButtonStyle,
  TextChannel,
  ComponentType,
} from "discord.js";
import type { Email } from "./gmailService";
import dotenv from "dotenv";

dotenv.config();

const client = new Client({
  intents: [GatewayIntentBits.Guilds, GatewayIntentBits.GuildMessages],
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

export async function sendForApproval(
  original: Email,
  draft: string
): Promise<boolean> {
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

  const row = new ActionRowBuilder<ButtonBuilder>().addComponents(
    new ButtonBuilder()
      .setCustomId("approve")
      .setLabel("Approve & Send")
      .setStyle(ButtonStyle.Success),
    new ButtonBuilder()
      .setCustomId("reject")
      .setLabel("Reject")
      .setStyle(ButtonStyle.Danger)
  );

  const message = await channel.send({
    content: [
      `**New email from:** ${original.from}`,
      `**Subject:** ${original.subject}`,
      `**Date:** ${original.date}`,
      "",
      "---",
      "**Original:**",
      `> ${original.body.slice(0, 500)}${original.body.length > 500 ? "..." : ""}`,
      "",
      "**Drafted Reply:**",
      draft,
      "---",
    ].join("\n"),
    components: [row],
  });

  return new Promise((resolve) => {
    const collector = message.createMessageComponentCollector({
      componentType: ComponentType.Button,
      time: 60 * 60 * 1000, // 1 hour timeout
    });

    collector.on("collect", async (interaction) => {
      if (interaction.customId === "approve") {
        await interaction.update({
          content: message.content + "\n\n**Status: APPROVED** ✅",
          components: [],
        });
        resolve(true);
      } else {
        await interaction.update({
          content: message.content + "\n\n**Status: REJECTED** ❌",
          components: [],
        });
        resolve(false);
      }
      collector.stop();
    });

    collector.on("end", (collected) => {
      if (collected.size === 0) {
        message.edit({
          content: message.content + "\n\n**Status: TIMED OUT** ⏰",
          components: [],
        });
        resolve(false);
      }
    });
  });
}
