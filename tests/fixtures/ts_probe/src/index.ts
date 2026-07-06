import axios from "axios";
import { Client } from "pg";
import * as fs from "fs";
import { exec } from "child_process";

export async function fetchUser(id: string): Promise<string> {
  const res = await axios.get(`https://api.example.com/users/${id}`);
  return res.data;
}

export function readConfig(path: string): string {
  return fs.readFileSync(path, "utf8");
}

export async function queryDb(): Promise<void> {
  const client = new Client();
  await client.query("SELECT 1");
}

export function runCmd(): void {
  exec("ls");
  const home = process.env.HOME;
  console.log(home);
}
