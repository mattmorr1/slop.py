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

export function sumA(xs: number[]): number {
  let total = 0;
  for (const x of xs) {
    if (x > 0) { total += x * 2; }
  }
  return total;
}

export function sumB(xs: number[]): number {
  let total = 0;
  for (const x of xs) {
    if (x > 0) { total += x * 2; }
  }
  return total;
}

export function tangled(m: number[][], cfg: any): number {
  let n = 0;
  for (const row of m) {
    for (const v of row) {
      if (v > 0 && v < 100) {
        while (n < v) {
          if (n % 2 === 0 || n % 3 === 0) { n++; }
          else if (n > cfg.max && cfg.on) { break; }
        }
      }
    }
  }
  if (n > 10 || n < -10) { return -1; }
  return n;
}
