//
// Copyright (c) 2010-2019 Antmicro
//
// This file is licensed under the MIT License.
// Full license text is available in 'licenses/MIT.txt'.
//
using System.Collections.Generic;
using Antmicro.Renode.Core;
using Antmicro.Renode.Core.Structure;
using Antmicro.Renode.Core.Structure.Registers;
using Antmicro.Renode.Logging;
using Antmicro.Renode.Peripherals.Bus;
using Antmicro.Renode.Utilities.Collections;

namespace Antmicro.Renode.Peripherals.SPI
{
    public class ComSoC : NullRegistrationPointPeripheralContainer<ISPIPeripheral>, IDoubleWordPeripheral, IKnownSize
    {
        public ComSoC(Machine machine) : base(machine)
        {
            IRQ = new GPIO();

            var regs = new Dictionary<long, DoubleWordRegister>
            {
                { (long)Registers.Tx, new DoubleWordRegister(this)
                    .WithValueField(0, 16, writeCallback: (_, value) => SendData(value), name: "TX")
                    .WithReservedBits(16, 16)
                },
                { (long)Registers.Rx, new DoubleWordRegister(this)
                    .WithValueField(0, 16, valueProviderCallback: _ => (uint)lastRxValue, name: "RX")
                    .WithReservedBits(16, 16)
                },
                { (long)Registers.Control, new DoubleWordRegister(this)
                    .WithFlag(0, out irqOnFinished, writeCallback: (_, __) => UpdateInterrupts(), name: "INTENA")
                    .WithFlag(1, out blockTxOnHold, name: "AUTOHOLD")
                    .WithReservedBits(2, 30)
                },
                { (long)Registers.Status, new DoubleWordRegister(this)
                    .WithFlag(0, out transactionInProgress, name: "TIP")
                    .WithFlag(1, out transactionHold, name: "HOLD", valueProviderCallback: _ => HOLD.IsSet)
                    .WithReservedBits(2, 30)
                },
                { (long)Registers.EvStatus, new DoubleWordRegister(this)
                    .WithFlag(0, FieldMode.Read, name: "SPI_INT", valueProviderCallback: _ => irqSpiIntStatus)
                    .WithFlag(1, FieldMode.Read, name: "SPI_HOLD", valueProviderCallback: _ => irqSpiHoldStatus)
                    .WithReservedBits(2, 30)
                },

                { (long)Registers.EvPending, new DoubleWordRegister(this)
                    .WithFlag(0, out irqSpiIntPending, FieldMode.Read | FieldMode.WriteOneToClear, name: "SPI_INT", changeCallback: (_, __) => UpdateInterrupts())
                    .WithFlag(1, out irqSpiHoldPending, FieldMode.Read | FieldMode.WriteOneToClear, name: "SPI_HOLD", changeCallback: (_, __) => UpdateInterrupts())
                    .WithReservedBits(2, 30)
                },

                { (long)Registers.EvEnable, new DoubleWordRegister(this)
                    .WithFlag(0, out irqSpiIntEnabled, name: "SPI_INT", changeCallback: (_, __) => UpdateInterrupts())
                    .WithFlag(1, out irqSpiIntEnabled, name: "SPI_HOLD", changeCallback: (_, __) => UpdateInterrupts())
                    .WithReservedBits(2, 30)
                },
            };
            registers = new DoubleWordRegisterCollection(this, regs);
        }

        public uint ReadDoubleWord(long offset)
        {
            return registers.Read(offset);
        }

        public void WriteDoubleWord(long offset, uint value)
        {
            registers.Write(offset, value);
        }

        public override void Reset()
        {
            registers.Reset();
            IRQ.Unset();
        }


        private void SendData(uint value)
        {
            var shortValue = (ushort)value;
            if (value != shortValue)
            {
                this.Log(LogLevel.Warning, "Trying to send 0x{0:X}, but it doesn't fit in a shirt. Will send 0x{1:X} instead", value, shortValue);
            }
            SendShort(shortValue);
        }

        private void SendShort(ushort value)
        {
            if (RegisteredPeripheral == null)
            {
                this.Log(LogLevel.Warning, "Trying to write 0x{0:X} to a slave peripheral, but nothing is connected");
                return;
            }
            transactionInProgress.Value = true;
            if (blockTxOnHold.Value == true && HOLD.IsSet)
            {
                this.Log(LogLevel.Warning, "Peripheral asserted HOLD -- skipping send");
                return;
            }

            lastRxValue = (int)RegisteredPeripheral.Transmit((byte)(value & 0xff));
            lastRxValue |= ((int)RegisteredPeripheral.Transmit((byte)(value >> 8 & 0xff)) << 8) & 0xff00;
            transactionInProgress.Value = false;
            if (irqOnFinished.Value)
            {
                this.irqSpiIntStatus = true;
                UpdateInterrupts();
                this.irqSpiIntStatus = false;
            }
            this.Log(LogLevel.Noisy, "Transmitted deferred data 0x{0:X}, received 0x{1:X}", value, lastRxValue);
        }

        private void UpdateInterrupts()
        {
            if (this.irqSpiHoldStatus && this.irqSpiHoldEnabled.Value)
            {
                this.irqSpiHoldPending.Value = true;
            }
            if (this.irqSpiIntStatus && this.irqSpiIntEnabled.Value)
            {
                this.irqSpiIntPending.Value = true;
            }
            IRQ.Set((this.irqSpiIntPending.Value && this.irqSpiIntEnabled.Value)
            || (this.irqSpiHoldPending.Value && this.irqSpiHoldEnabled.Value));
        }


        public long Size { get { return 4096; } }
        public GPIO IRQ { get; private set; }
        public GPIO HOLD { get; set; }
        private DoubleWordRegisterCollection registers;

        private IFlagRegisterField irqOnFinished;
        private IFlagRegisterField blockTxOnHold;
        private IFlagRegisterField transactionInProgress;
        private IFlagRegisterField transactionHold;
        private int lastRxValue;

        private IFlagRegisterField irqSpiIntEnabled;
        private IFlagRegisterField irqSpiIntPending;
        private bool irqSpiIntStatus;
        private IFlagRegisterField irqSpiHoldEnabled;
        private IFlagRegisterField irqSpiHoldPending;
        private bool irqSpiHoldStatus;

        // We can transmit 16 bits at a time, and there is no FIFO. Renode
        // transfers SPI data in blocks of 8 bits, so treat it as a FIFO
        // with a capacity of 2.
        private const int FifoCapacity = 2;

        public enum Registers
        {
            Tx = 0x0,
            Rx = 0x4,
            Control = 0x8,
            Status = 0xC,
            EvStatus = 0x10,
            EvPending = 0x14,
            EvEnable = 0x18,
        }
    }
}
