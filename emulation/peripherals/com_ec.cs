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
using Antmicro.Renode.Peripherals.SPI;
using Antmicro.Renode.Utilities.Collections;

namespace Antmicro.Renode.Peripherals.SPI
{
    public class ComEc : ISPIPeripheral, IDoubleWordPeripheral, IKnownSize
    {
        // 0xD0000000
        public ComEc(Machine machine)
        {
            IRQ = new GPIO();

            var regs = new Dictionary<long, DoubleWordRegister>
            {
                { (long)Registers.Control, new DoubleWordRegister(this)
                    .WithFlag(0,  writeCallback: (_, __) => UpdateInterrupts(), name: "CLRERR")
                    .WithFlag(1, writeCallback: (_, __) => {}, name: "HOST_INT")
                    .WithFlag(2, writeCallback: (_, __) => {}, name: "RESET")
                    .WithReservedBits(3, 29)
                },
                { (long)Registers.Status, new DoubleWordRegister(this)
                    .WithFlag(0, out transactionInProgress, name: "TIP")
                    .WithFlag(1, out rxAvailable, name: "RX_AVAIL")
                    .WithFlag(2, out rxOverflow, name: "RX_OVER")
                    .WithFlag(3, out rxUnderflow, name: "RX_UNDER")
                    .WithFlag(16, out txAvailable, name: "TX_AVAIL")
                    .WithFlag(17, out txEmpty, name: "TX_EMPTY")
                    .WithFlag(30, out txOverflow, name: "TX_OVER")
                    .WithFlag(31, out txUnderflow, name: "TX_UNDER")
                },
                { (long)Registers.EvStatus, new DoubleWordRegister(this)
                    .WithFlag(0, FieldMode.Read, name: "SPI_AVAIL", valueProviderCallback: _ => irqSpiAvailStatus)
                    .WithFlag(1, FieldMode.Read, name: "SPI_EVENT", valueProviderCallback: _ => irqSpiEventStatus)
                    .WithFlag(2, FieldMode.Read, name: "SPI_ERR", valueProviderCallback: _ => irqSpiErrStatus)
                    .WithReservedBits(3, 29)
                },

                { (long)Registers.EvPending, new DoubleWordRegister(this)
                    .WithFlag(0, out irqSpiAvailPending, FieldMode.Read | FieldMode.WriteOneToClear, name: "SPI_AVAIL", changeCallback: (_, __) => UpdateInterrupts())
                    .WithFlag(1, out irqSpiEventPending, FieldMode.Read | FieldMode.WriteOneToClear, name: "SPI_EVENT", changeCallback: (_, __) => UpdateInterrupts())
                    .WithFlag(2, out irqSpiErrPending, FieldMode.Read | FieldMode.WriteOneToClear, name: "SPI_ERR", changeCallback: (_, __) => UpdateInterrupts())
                    .WithReservedBits(3, 29)
                },

                { (long)Registers.EvEnable, new DoubleWordRegister(this)
                    .WithFlag(0, out irqSpiAvailEnabled, name: "SPI_AVAIL", changeCallback: (_, __) => UpdateInterrupts())
                    .WithFlag(1, out irqSpiEventEnabled, name: "SPI_EVENT", changeCallback: (_, __) => UpdateInterrupts())
                    .WithFlag(2, out irqSpiErrEnabled, name: "SPI_ERR", changeCallback: (_, __) => UpdateInterrupts())
                    .WithReservedBits(3, 29)
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

        public void Reset()
        {
            registers.Reset();
            IRQ.Unset();
        }

//            machine.SystemBus.ReadBytes(bufferAddress, newbuf.Length, newbuf, 0);

        // private void SendData(uint value)
        // {
        //     var shortValue = (ushort)value;
        //     if (value != shortValue)
        //     {
        //         this.Log(LogLevel.Warning, "Trying to send 0x{0:X}, but it doesn't fit in a shirt. Will send 0x{1:X} instead", value, shortValue);
        //     }
        //     SendShort(shortValue);
        // }

        // private void SendShort(ushort value)
        // {
        //     if (RegisteredPeripheral == null)
        //     {
        //         this.Log(LogLevel.Warning, "Trying to write 0x{0:X} to a slave peripheral, but nothing is connected");
        //         return;
        //     }
        //     transactionInProgress.Value = true;
        //     if (blockTxOnHold.Value == true && HOLD.IsSet)
        //     {
        //         this.Log(LogLevel.Warning, "Peripheral asserted HOLD -- skipping send");
        //         return;
        //     }

        //     lastRxValue = (int)RegisteredPeripheral.Transmit((byte)(value & 0xff));
        //     lastRxValue |= ((int)RegisteredPeripheral.Transmit((byte)(value >> 8 & 0xff)) << 8) & 0xff00;
        //     transactionInProgress.Value = false;
        //     if (irqOnFinished.Value)
        //     {
        //         this.irqSpiIntStatus = true;
        //         UpdateInterrupts();
        //         this.irqSpiIntStatus = false;
        //     }
        //     this.Log(LogLevel.Noisy, "Transmitted deferred data 0x{0:X}, received 0x{1:X}", value, lastRxValue);
        // }

        private void UpdateInterrupts()
        {
            if (this.irqSpiAvailStatus && this.irqSpiAvailEnabled.Value)
            {
                this.irqSpiAvailPending.Value = true;
            }
            if (this.irqSpiEventStatus && this.irqSpiEventEnabled.Value)
            {
                this.irqSpiEventPending.Value = true;
            }
            if (this.irqSpiErrStatus && this.irqSpiErrEnabled.Value)
            {
                this.irqSpiErrPending.Value = true;
            }
            IRQ.Set((this.irqSpiAvailPending.Value && this.irqSpiAvailEnabled.Value)
            || (this.irqSpiEventPending.Value && this.irqSpiEventEnabled.Value)
            || (this.irqSpiErrPending.Value && this.irqSpiErrEnabled.Value));
        }


        public void FinishTransmission()
        {
            Reset();
        }

        public byte Transmit(byte data)
        {
            // byte value = 0;
            // if(isFirstByte)
            // {
            //     currentReadOut = (uint)Temperature * 8;
            //     value = (byte)(currentReadOut >> 8);
            // }
            // else
            // {
            //     value = (byte)(currentReadOut & 0xFF);
            // }
            // isFirstByte = !isFirstByte;
            // return value;
            return 0xaa;
        }


        public long Size { get { return 4096; } }
        public GPIO IRQ { get; private set; }
        public GPIO HOLD { get; set; }
        private DoubleWordRegisterCollection registers;

        private IFlagRegisterField transactionInProgress;
        private IFlagRegisterField rxAvailable;
        private IFlagRegisterField rxOverflow;
        private IFlagRegisterField rxUnderflow;
        private uint rxLevel;
        private IFlagRegisterField txAvailable;
        private IFlagRegisterField txEmpty;
        private uint txLevel;
        private IFlagRegisterField txOverflow;
        private IFlagRegisterField txUnderflow;
        private IFlagRegisterField irqSpiAvailEnabled;
        private IFlagRegisterField irqSpiAvailPending;
        private bool irqSpiAvailStatus;
        private IFlagRegisterField irqSpiEventEnabled;
        private IFlagRegisterField irqSpiEventPending;
        private bool irqSpiEventStatus;
        private IFlagRegisterField irqSpiErrEnabled;
        private IFlagRegisterField irqSpiErrPending;
        private bool irqSpiErrStatus;
        private readonly uint bufferAddress = 0xD0000000;

        public enum Registers
        {
            Control = 0x0,
            Status = 0x4,
            EvStatus = 0x08,
            EvPending = 0x0c,
            EvEnable = 0x10,
        }
    }
}
