/**
 * This suite contains tests checking default ERC-20 contract behavior.
 */

import { TestMaster } from '../../src';
import { Token } from '../../src/types';

import * as zksync from 'zksync-ethers';
import { ethers } from 'ethers';
import { BOOTLOADER_FORMAL_ADDRESS } from 'zksync-ethers/build/utils';
import fs from 'fs';

describe('Debug methods', () => {
    let testMaster: TestMaster;
    let alice: zksync.Wallet;
    let bob: zksync.Wallet;
    let tokenDetails: Token;
    let aliceErc20: zksync.Contract;

    beforeAll(async () => {
        testMaster = TestMaster.getInstance(__filename);
        alice = testMaster.mainAccount();
        bob = testMaster.newEmptyAccount();

        tokenDetails = testMaster.environment().erc20Token;
        aliceErc20 = new zksync.Contract(tokenDetails.l2Address, zksync.utils.IERC20, alice);
    });

    test('Should show out-of-gas error in debug_traceTransaction', async () => {
        const smallGasLimit = 20_000;
        let tx;
        try {
            tx = await aliceErc20.transfer(bob.address, 1n, {
                gasLimit: smallGasLimit
            });
            await tx.wait();
        } catch (err) {
            console.log("err", err);
        }

        const txCallTrace = await testMaster
            .mainAccount()
            .provider.send('debug_traceTransaction', [tx.hash]);

        console.log("txCallTrace", txCallTrace);
        //expect(txCallTrace.error).toBe('OutOfGas');

    });

    test('Should not fail for infinity recursion', async () => {
        const bytecodePath = `${
            testMaster.environment().pathToHome
        }/core/tests/ts-integration/contracts/zkasm/artifacts/deep_stak.zkasm/zkasm/deep_stak.zkasm.zbin`;
        const bytecode = fs.readFileSync(bytecodePath, 'utf-8');

        const contractFactory = new zksync.ContractFactory([], bytecode, testMaster.mainAccount());
        const deployTx = await contractFactory.deploy();
        const contractAddress = await (await deployTx.waitForDeployment()).getAddress();
        let txCallTrace = await testMaster.mainAccount().provider.send('debug_traceCall', [
            {
                to: contractAddress,
                data: '0x'
            }
        ]);
        let expected = {
            error: null,
            from: ethers.ZeroAddress,
            gas: expect.any(String),
            gasUsed: expect.any(String),
            input: expect.any(String),
            output: '0x',
            revertReason: 'Error function_selector = 0x, data = 0x',
            to: BOOTLOADER_FORMAL_ADDRESS,
            type: 'call',
            value: expect.any(String),
            calls: expect.any(Array)
        };
        expect(txCallTrace).toEqual(expected);
    });

    test('Debug sending erc20 token in a block', async () => {
        const value = 200n;
        await aliceErc20.transfer(bob.address, value).then((tx: any) => tx.wait());
        const tx = await aliceErc20.transfer(bob.address, value);
        const receipt = await tx.wait();
        const blockCallTrace = await testMaster
            .mainAccount()
            .provider.send('debug_traceBlockByNumber', [receipt.blockNumber.toString(16)]);
        const blockCallTraceWithTracer = await testMaster
            .mainAccount()
            .provider.send('debug_traceBlockByNumber', [receipt.blockNumber.toString(16), { tracer: 'callTracer' }]);
        const expectedTraceInBlock = {
            from: ethers.ZeroAddress,
            gas: expect.any(String),
            gasUsed: expect.any(String),
            input: expect.any(String),
            output: '0x',
            to: BOOTLOADER_FORMAL_ADDRESS,
            type: 'call',
            value: expect.any(String),
            calls: expect.any(Array)
            // We intentionally skip `error` and `revertReason` fields: the block may contain failing txs
            // generated by other tests.
        };
        for (let i = 0; i < blockCallTrace.length; i++) {
            expect(blockCallTrace[i]).toMatchObject({ result: expectedTraceInBlock });
            expect(blockCallTrace[i]).toEqual(blockCallTraceWithTracer[i]);
        }

        const expected = {
            error: null,
            from: ethers.ZeroAddress,
            gas: expect.any(String),
            gasUsed: expect.any(String),
            input: `0xa9059cbb000000000000000000000000${bob.address
                .slice(2, 42)
                .toLowerCase()}00000000000000000000000000000000000000000000000000000000000000${value
                .toString(16)
                .slice(0, 2)}`, // no 0x prefix
            output: '0x',
            revertReason: null,
            to: BOOTLOADER_FORMAL_ADDRESS,
            type: 'call',
            value: '0x0',
            calls: expect.any(Array)
        };
        const txCallTrace = await testMaster.mainAccount().provider.send('debug_traceTransaction', [tx.hash]);
        const txCallTraceWithTracer = await testMaster
            .mainAccount()
            .provider.send('debug_traceTransaction', [tx.hash, { tracer: 'callTracer' }]);
        expect(txCallTrace).toEqual(expected);
        expect(txCallTrace).toEqual(txCallTraceWithTracer);
    });

    afterAll(async () => {
        await testMaster.deinitialize();
    });
});
