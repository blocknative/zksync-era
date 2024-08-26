import { spawn as _spawn, ChildProcessWithoutNullStreams, type ProcessEnvOptions } from 'child_process';
import path from 'path';
import * as fs from 'fs';

const pathToHome = path.join(__dirname, '../../../..');
export const slugify = (...args: string[]): string => {
    const value = args.join(' ');

    return value
        .normalize('NFD') // split an accented letter in the base letter and the acent
        .replace(/[\u0300-\u036f]/g, '') // remove all previously split accents
        .toLowerCase()
        .trim()
        .replace(/[^a-z0-9 ]/g, '') // remove all chars not letters, numbers and spaces (to be replaced)
        .replace(/\s+/g, '-'); // separator
};
// executes a command in background and returns a child process handle
// by default pipes data to parent's stdio but this can be overridden
export function background({
    command,
    stdio = 'inherit',
    cwd,
    env
}: {
    command: string;
    stdio: any;
    cwd?: ProcessEnvOptions['cwd'];
    env?: ProcessEnvOptions['env'];
}): ChildProcessWithoutNullStreams {
    command = command.replace(/\n/g, ' ');
    console.log(`Running command in background: ${command}`);
    let command_answer = slugify(command);
    command_answer = command_answer.substring(Math.max(command_answer.length - 30, 0), command_answer.length);

    let process = _spawn(command, {
        stdio: ['pipe', 'pipe', 'pipe'],
        shell: true,
        detached: true,
        cwd,
        env
    });

    process.stdout.on('data', (data) => {
        let file = path.join(pathToHome, command_answer);
        fs.appendFileSync(file, data);
    });
    process.stderr.on('data', (data) => {
        let file = path.join(pathToHome, command_answer);
        fs.appendFileSync(file, data);
    });
    return process;
}

export function runInBackground({
    command,
    components,
    stdio,
    cwd,
    env
}: {
    command: string;
    components?: string[];
    stdio: any;
    cwd?: Parameters<typeof background>[0]['cwd'];
    env?: Parameters<typeof background>[0]['env'];
}): ChildProcessWithoutNullStreams {
    if (components && components.length > 0) {
        command += ` --components=${components.join(',')}`;
    }

    return background({
        command,
        stdio,
        cwd,
        env
    });
}

export function runExternalNodeInBackground({
    components,
    stdio,
    cwd,
    env,
    useZkInception,
    chain
}: {
    components?: string[];
    stdio: any;
    cwd?: Parameters<typeof background>[0]['cwd'];
    env?: Parameters<typeof background>[0]['env'];
    useZkInception?: boolean;
    chain?: string;
}): ChildProcessWithoutNullStreams {
    let command = '';
    if (useZkInception) {
        command = 'zk_inception external-node run';
        command += chain ? ` --chain ${chain}` : '';
        // const basePath = `${pathToHome}/chains/${chain}/configs/external_node`;
        // const config_path = `${basePath}/general.yaml`;
        // const secrets_path = `${basePath}/secrets.yaml`;
        // const en_config_path = `${basePath}/external_node.yaml`;
        //
        // command = `cargo run --release --bin zksync_external_node --
        // --config-path ${config_path}
        // --secrets-path ${secrets_path}
        // --external-node-config-path ${en_config_path}`;
    } else {
        command = 'zk external-node --';

        const enableConsensus = process.env.ENABLE_CONSENSUS === 'true';
        if (enableConsensus) {
            command += ' --enable-consensus';
        }
    }
    return runInBackground({ command, components, stdio, cwd, env });
}
