import { getMiNA } from "@mi-gpt/miot";
import { createHash } from "node:crypto";
import * as fs from "node:fs";
import * as path from "node:path";
import * as stream from "node:stream";
import { promisify } from "node:util";

const kSupportedDevices = [
  "LX06", // 小爱音箱 Pro
  "OH2P", // Xiaomi 智能音箱 Pro
];

// 获取 OTA 信息
async function getOTA(channel: "release" | "current" | "stable" = "release") {
  const MiNA = await getMiNA({
    did: process.env.MI_DID!,
    userId: process.env.MI_USER!,
    password: process.env.MI_PASS!,
    passToken: process.env.MI_TOKEN,
    debug: !!process.env.MI_DEBUG,
  });
  if (!MiNA) {
    return;
  }

  const devices = await MiNA.getDevices();
  const speaker = MiNA.account.device as any;

  if (!kSupportedDevices.includes(speaker.hardware)) {
    console.log(
      `❌ 暂不支持当前设备型号: ${speaker.hardware}（${speaker.name}）`
    );
    console.log(`可用设备列表：`);
    console.log(
      JSON.stringify(
        devices
          .filter((e: any) => kSupportedDevices.includes(e.hardware))
          .map((e: any) => ({
            name: e.name,
            did: e.miotDID,
          })),
        null,
        4
      )
    );
    return;
  }

  const model = speaker.hardware;
  const time = Date.now();
  const sn = process.env.DEBUG_VERSION ? "" : speaker.serialNumber;
  const version = process.env.DEBUG_VERSION ?? speaker.romVersion;
  const otaInfo = `channel=${channel}&filterID=${sn}&locale=zh_CN&model=${model}&time=${time}&version=${version}&8007236f-a2d6-4847-ac83-c49395ad6d65`;
  const base64Str = Buffer.from(otaInfo).toString("base64");
  const code = createHash("md5").update(base64Str).digest("hex");

  return {
    sn,
    model,
    version,
    url: `http://api.miwifi.com/rs/grayupgrade/v2/${model}?model=${model}&version=${version}&channel=${channel}&filterID=${sn}&locale=zh_CN&time=${time}&s=${code}`,
  };
}

async function main() {
  if (!process.env.DEBUG_VERSION) {
    console.log(`🔥 正在获取设备信息...`);
  }
  let ota: any = {};
  if (process.env.OTA) {
    ota = JSON.parse(process.env.OTA);
  } else {
    ota = await getOTA();
  }
  if (!ota?.url) {
    console.log(`❌ 获取设备信息失败`);
    process.exit(1);
  }
  if (process.env.DEBUG_VERSION) {
    console.log(JSON.stringify(ota));
    return;
  }
  console.log(`🔥 正在获取 OTA 信息...`);
  const res = await fetch(ota.url);
  const data = await res.json();
  if (data.code === "0" && data.data) {
    if (data.data.currentInfo) {
      console.log("\n=== 当前版本固件 ===\n");
      const filePath = await downloadFirmware(data.data.currentInfo);
      if (filePath) {
        fs.writeFileSync(
          path.join(process.cwd(), "assets", ".model"),
          ota.model.toUpperCase()
        );
        fs.writeFileSync(
          path.join(process.cwd(), "assets", ".version"),
          ota.version
        );
      }
    }
  } else {
    console.log(`❌ 获取固件信息失败: ${data.code || "未知错误"}`);
    process.exit(1);
  }
}

main();

// 下载固件
async function downloadFirmware(firmware: {
  link: string;
  hash: string;
  toVersion?: string;
  size?: number;
}): Promise<string | undefined> {
  if (!firmware || !firmware.link) {
    console.log("❌ 无效的固件信息");
    return;
  }

  const assetsDir = path.join(process.cwd(), "assets");

  // 从链接中提取文件名
  const url = new URL(firmware.link);
  const filename = path.basename(url.pathname);

  // 打印固件信息
  console.log(`- 版本: ${firmware.toVersion || "未知"}`);
  console.log(
    `- 大小: ${
      firmware.size ? (firmware.size / 1024 / 1024).toFixed(2) + "MB" : "未知"
    }`
  );
  console.log(`- 文件: ${filename}`);
  console.log(`- MD5: ${firmware.hash || "未知"}\n`);

  try {
    const filePath = await downloadFile(firmware.link, assetsDir, filename);
    return filePath;
  } catch (error) {
    console.error(`❌ 下载固件失败: ${error}`);
    return;
  }
}

async function ensureDir(dirPath: string): Promise<void> {
  try {
    await fs.promises.access(dirPath);
  } catch (error) {
    await fs.promises.mkdir(dirPath, { recursive: true });
  }
}

async function downloadFile(
  url: string,
  destDir: string,
  filename: string
): Promise<string> {
  await ensureDir(destDir);

  const destPath = path.join(destDir, filename);

  // 检查文件是否已存在
  try {
    await fs.promises.access(destPath);
    console.log(`ℹ️ 文件已存在: ${destPath}`);
    return destPath;
  } catch (error) {
    // 文件不存在，下载它
    console.log(`⬇️ 开始下载: ${url}`);

    const response = await fetch(url);
    if (!response.ok) {
      throw new Error(`下载失败: ${response.status} ${response.statusText}`);
    }

    // 获取文件大小
    const contentLength = response.headers.get("content-length");
    const totalSize = contentLength ? parseInt(contentLength, 10) : 0;

    // 创建文件写入流
    const fileStream = fs.createWriteStream(destPath);

    if (!response.body) {
      throw new Error("下载失败: 无法获取响应体");
    }

    // 创建进度显示变量
    let downloadedBytes = 0;
    let lastLoggedPercent = -1;
    let startTime = Date.now();

    // 使用 TransformStream 来跟踪下载进度
    const progressStream = new stream.Transform({
      transform(chunk, encoding, callback) {
        downloadedBytes += chunk.length;

        if (totalSize) {
          const percent = Math.floor((downloadedBytes / totalSize) * 100);

          // 确保每增加1%才打印一次，避免日志过多
          if (percent > lastLoggedPercent) {
            lastLoggedPercent = percent;

            // 格式化输出
            const downloaded = (downloadedBytes / 1024 / 1024).toFixed(2);
            const total = (totalSize / 1024 / 1024).toFixed(2);

            // 使用\r使光标回到行首，实现同行更新
            process.stdout.write(
              `\r下载进度: ${percent}% | ${downloaded}MB/${total}MB`
            );

            // 如果下载完成，打印换行符
            if (downloadedBytes === totalSize) {
              process.stdout.write("\n");
            }
          }
        } else {
          // 无法获取总大小时，只显示已下载大小
          if (downloadedBytes % (1024 * 1024) === 0) {
            // 每1MB打印一次
            const downloaded = (downloadedBytes / 1024 / 1024).toFixed(2);
            process.stdout.write(`\r已下载: ${downloaded}MB`);
          }
        }

        this.push(chunk);
        callback();
      },
    });

    // 完成后清理并打印最终结果
    progressStream.on("end", () => {
      const totalTime = ((Date.now() - startTime) / 1000).toFixed(2);
      console.log(`\n✅ 下载完成: ${destPath}`);
      console.log(
        `   总大小: ${(downloadedBytes / 1024 / 1024).toFixed(
          2
        )}MB, 用时: ${totalTime}秒`
      );
    });

    // 使用pipeline连接流
    await promisify(stream.pipeline)(
      // @ts-ignore - response.body 在 Node.js 中是 ReadableStream
      response.body,
      progressStream,
      fileStream
    );

    return destPath;
  }
}
